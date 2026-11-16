# Thread usage review (2026-11-17)

Status: findings only. The owner will decide on the backlog speed-up on another day. Nothing is built.

## Analysis


## 1. Thread map (file : function)
| Work | Thread |
|---|---|
| Socket read, MQTT packet decode (ntex), build `Incoming` (topic `Arc<str>`, payload `Bytes::copy_from_slice`, v5 props to `String`s), bytes budget, `try_send` | `mqtt-net` (tokio current_thread). `net/wire.rs: incoming()/incoming_v3()`, `net.rs: Ingest::queue` |
| Topic UTF-8/validity | ntex codec on `mqtt-net`. Level-depth check (`MAX_TOPIC_LEVELS`) is in `store.rs: insert` on the UI thread |
| Queue net -> UI | `mpsc` 50k msgs and 32 MiB (`NetLimits`). Full = drop + count. `net.rs:449,1618` |
| Drain loop, merge, `TopicStore::insert` (node walk/create, 3 `Msg` copies sharing one `Bytes`, history ring, counts to root, list row, row splice), eviction, `work()` (tree/list filter rebuild, snapshot, row trim) | gpui main. `ui.rs: spawn_ingest, merge`; `store.rs: insert, work, evict_step, list_step, tree_step`. 4 ms/frame (3 ms merge + 1 ms reserve for work), 33 ms coalesce |
| Per-new-message list filter test (topic + first 1 KiB payload, `from_utf8_lossy`) and tree-filter test of new node | main, inside `insert` / `admit_row` (`store.rs: row_matches, tree_test`) |
| Filter rebuild over old rows/nodes | main, resumable in 256-item steps (`list_step/tree_step`), shares the 4 ms. Filter text debounce 150 ms (`ui.rs: debounce_filter`, a timer started by input) |
| Visible-row list: `store.rows()` + `splice_new_row/splice_toggle` (incremental, 48 us for 140 rows, 256 us for 10k rows) | main |
| Previews (`decode::preview`, 240/80/120 chars) | main, in render, visible rows only (`ui.rs:3012,3223`, `details.rs:1310`) |
| Details payload decode, JSON pretty-print, auto-detect, hash (`decode::render`) | gpui background executor, cancel flag, new selection cancels (`details.rs:500`). Result back via `render_done`; editor text set on main (`ui_step grow`) |
| Composer "Format JSON" | background executor (`compose.rs:586`) |
| Publish: validate on UI (`publish::validate_outgoing` is cheap), `Net::send` try_send to command queue, `publish_op` on `mqtt-net`, `record_sent` row on UI | main + `mqtt-net` |
| Composer history/drafts (`composer.rs History`) | main (small, in memory); persisted through settings writer |
| Settings JSON write (500 ms debounce), atomic | `settings-writer` thread (`persist.rs`) |
| Keyring | one `keyring` thread (`secrets.rs`) |
| Log file | `log-writer` thread (`logging.rs`) |
| TLS material load/connector, DNS | tokio `spawn_blocking` on `mqtt-net` runtime (`net.rs:1846`, `net/dial.rs:93`) |
| Update check/download, desktop entry, dir create, profile import/export file I/O | gpui background executor |
Other threads seen: gpui `Worker-0..5` (one per core, idle), `smol`/`blocking`, zbus x3, clipboard, `llvmpipe-0..5` (software rasteriser, Xvfb only), one unnamed gpui render thread, `WSI swapchain`.

## 2. Measurements
Setup: release binary, Xvfb 1600x1000 llvmpipe, broker Mosquitto 127.0.0.1:18830, flood 100 B payload, 10k topics unless noted, `MQTT_NAVIGATOR_METRICS=1`, 6 cores. perf is NOT usable (`perf_event_paranoid=4`, perf record fails). Used app metrics plus /proc/<pid>/task/*/stat ticks over a 12 s window. One `flood` process tops out at about 25k msg/s, so 50k and 75k use 2 and 3 floods. Logs: `<name>.log`, `<name>.cpu`, `<name>.flood*`, screenshots `<name>.png` (filter and tree keys checked in the screenshots).

| Run | merge p95 / max (ms) | main CPU | mqtt-net CPU | drops | note |
|---|---|---|---|---|---|
| 10k, list | 0.66 or 2.1 (bimodal: 2.1 = eviction work active) / 2.3 | 9% | 3.7% | 0 | |
| 25k, list (56 s, store steady at 117-123 MiB) | 3.3 / 3.7 | 12% | 8.8% | 0 | merge at its 3 ms budget |
| 50k (2 floods), 24 s | 3.4 / 3.9 | 13.6% | 16% | 192k at end (store full at 157 MiB) | drops start once the store reaches budget+25% |
| 75k (3 floods) | 3.4-3.5 / 3.7 | 13% | 21% | 845k | about 1.2M of 1.8M sent merged; the rest are queue drops, most merged ones are store-admission drops |
| 10k + text filter | 1.1-3.3 | 6.5% | 3.7% | 0 | filter cost is not visible |
| 25k + text filter | 3.3 / 4.0 | 9.4% | 8.8% | 0 | same as no filter |
| 25k, Tree view (collapsed, 2 rows) | 3.4 / 4.2 | 9.5% | 9% | 0 | |
| 25k, 100k topics, list | 3.3 / 4.0 | 14.1% | 8.4% | 167 (topic cap/node budget) | topics plateau at about 70k (idle-leaf removal) |
| 25k, 100k topics, Tree | 3.3-3.6 / 4.0 | 10.8% | 8.4% | 0 | |
| 25k, 100k topics, filter | 3.5-3.96 / 4.4 | 11.2% | 8.5% | 7.6k | slightly worse, tail over 4 ms |
| 10k, 1000 B payload + filter | 2.4-3.3 | 8% | 5.9% | 0 | |
| 25k, 1000 B payload + filter | 3.5 / 4.0 | 10.8% | 14.4% | 140k | store budget hit (3 copies x 1 KB x 25k/s = churn) |
| store-only micro-test (`perf_sanity`, release) | insert of 100k new topics: 277 ms = 2.8 us/msg; list filter rebuild slice worst 1.4 ms; tree filter slice worst 4.0 ms (total 6.6 ms for 111k nodes) | | | | |

Frame numbers: `frame_p95` is a constant 48 ms (about 22 frames/s) in EVERY run, also at 10k. That is llvmpipe: 6 raster threads at 25% each (1.5 cores) plus swapchain. It is not our code. Our code on main: CPU 6-14% of one core = 60-140 ms/s; at 22 frames/s that is 3-6 ms per frame, of which merge+work is 0.7-3.4 ms and render preparation (rows, previews, layout) about 1.5-2.5 ms (3-4 ms at 100k topics). The frame_p95 of about 7.5 ms from phase 8 was on a real GPU path. The Xvfb frame time cannot show the 16.7 ms / 8.3 ms question; main-thread work per frame (about 4-6 ms) is the number to compare, and it is under both.

## 3. Bottlenecks
1. UI merge duty cycle is the ceiling. Merge runs once per 33 ms with a 3 ms budget (4 ms minus 1 ms work reserve) = 9-12% of the main thread. Cost is about 2 us/msg for insert and 4 us/msg with eviction at budget. So about 25-30k msg/s sustained once the store is full, and drops begin above that (first drops at 50k after about 10 s, when the store hits budget+25%). `mqtt-net` is not the limit: 3.5 us/msg total, 21% of one core at 75k, so about 300k msg/s capacity. Net queue (50k msgs) only fills at 75k.
2. Most drops at overload are store admission drops (`Insert::Dropped`, store at budget+25%) because eviction (`work()`, 1 ms reserve per frame, 256-item steps) cannot keep up with insert. Eviction + insert are both on the one UI thread.
3. The idle gap: with a backlog the loop still waits `COALESCE` 33 ms between frames, so merge uses at most 3-4 ms of each 33 ms. About 88% of the main thread is idle at 25k, yet drops occur at 50k. The limit is the budget rule, not CPU.
4. Frames: filter, tree and 100k topics add little (main CPU +0-3 points). Long frame tails come only from llvmpipe. Tree filter rebuild slice worst 4.0 ms (one `work` round of 256 nodes at 100k+ nodes) can slightly exceed the 4 ms target (max seen 4.4 ms).
5. Single-threaded but parallel-friendly: (a) rebuild scans for list/tree filter (read-only over rows, large), (b) payload to preview/filter-text/UTF-8 pre-check (pure function of the message), (c) decode-for-details (already background). Not parallel-friendly: TopicStore mutation (tree, counts to root, order maps, budget accounting), and one TCP connection read.

## 4. Recommendations, ranked benefit/cost
Rules recap: event-driven, no idle timers, bounded queues, <= 4 ms UI merge per frame, no blocking I/O on UI, core without gpui.

### R1 (cheap, biggest benefit/cost): use the idle main thread, not more threads
What: while a backlog exists, shorten the wait between merge slices. For example merge 3 ms, yield to render, repeat as soon as the frame is presented (or wait `COALESCE - merge` only for the notify, not for merging). Alternative: raise the budget when the queue is deep and no input is pending (still sliced, still <= 4 ms for each slice). Also give eviction a larger reserve when the store is above budget (it is what causes the drops).
Evidence: main thread at 9-14%; ceiling 25-30k/s is the 3 ms/33 ms duty; mqtt-net has 10x headroom.
Gain: expected 2-3x sustainable rate (about 60-80k msg/s) with no new thread and no ordering change. Frame rate stays the same because render is separate.
Risk: low. More frequent `cx.notify()` renders must stay at most one per frame (gpui coalesces). Must keep timer start-from-event (it already does).
Fit: keeps all rules (slice stays <= 4 ms).

### R2 (medium): decode/prepare on the net thread, before the queue
What: move per-message work that is pure and does not depend on the store into `mqtt-net`: compute `msg_cost`/`props_cost`, node-cost parts, topic level count (drop deep topics early), a cached "is UTF-8" flag and a 1 KiB lowercase-ready head for the text filter. The UI then only inserts.
Evidence: `mqtt-net` 3.5 us/msg now, UI insert 2-4 us/msg, of which the pure part (level count, `heap_bytes`, costs, `from_utf8_lossy` only when a filter is active) is small, estimate 10-20% of merge.
Gain: small (10-20% merge), a few-% net thread. Not worth it alone; do it only as part of R3.
Risk: low; order is kept (one thread, one FIFO). Budget accounting must stay in the store (needs current limits, which change live).

### R3 (high cost, real scaling): store thread + snapshots
What: move `TopicStore` to a dedicated store thread (exists only while an app session exists; parked on a channel, zero wake-ups when idle). Net thread -> store thread through the one bounded queue (FIFO = ordering and generation rule kept: the generation check moves to the store thread, and UI sends the current generation with a "reset" command). The UI sends commands (set filter, expand, pause, select) and receives immutable "view" snapshots: visible row range for list and tree, counts, stats. Snapshot is built on the store thread once per UI frame request (UI asks `want_view(range)` on event; no timer).
Evidence: UI merge is the only limit; a separate thread would give a whole core for insert+evict: expected 100-200k msg/s (insert 2-4 us/msg), merge no longer in frame budget, UI frame work falls to render only.
Risk: HIGH. The UI reads store synchronously in many places (`list_row`, `list_index_of`, `row_evicted`, anchors, `get(id)`, selection, details, history, retained checks, send record, palette). All become async or snapshot-based; scroll anchoring and selection need stable IDs (RowId and NodeId exist, good). Memory accounting stays in one owner (good: no cross-thread budget races). Snapshot copies cost memory and time (must be bounded: visible window plus margin). Determinism of tests changes. Pause semantics (frozen views) become simpler on a store thread.
Fit: AGENTS rules hold if the queue is bounded and the thread parks. It is a rewrite of the UI/store seam; plan as its own phase with its own gate.
Verdict: only if the product needs more than about 60-80k msg/s sustained. R1 first.

### R4 (low benefit): filter and tree rebuilds on the background executor with cancellation
What: copy-free parallel scan is hard because the store is `&mut` and owned by main. Rebuild would need either a snapshot of rows (copy 128 MiB worst) or R3.
Evidence: rebuilds are already resumable and sliced; filter ON made no measurable change at 10k/25k (merge p95 same, main CPU not higher); worst tree rebuild slice 4.0 ms at 111k nodes (total 6.6 ms), list rebuild 1.4 ms for 100k rows.
Gain: tiny today. A filter change on a full store takes only a few frames.
Risk: stale results vs live inserts (generation, then replay of inserts that happened during the scan), complexity.
Verdict: do not do without R3. If wanted, only reduce `WORK_CHUNK` for the tree step so a round is < 1 ms (4.0 -> 4.4 ms tail).

### R5: dynamic worker pool (size from `available_parallelism`, parked when idle, fan out under backlog)
- Where it can help: pure per-message prep (UTF-8 check, lowercase head, preview string, cost computation) and filter-rebuild scans over immutable snapshots.
- Order: split by topic hash (hash(topic) % N). Per-topic order is kept; cross-topic global order (the message list is newest-first by arrival) is NOT kept. To keep it, give each message a sequence number at the net thread and have the store thread (or the UI) merge by sequence: a reorder buffer, bounded by the queue size. Generation: each message carries its generation and a stale one is dropped at the merge, as now.
- Event-driven: workers block on a bounded channel (a `crossbeam`/`std` condvar), zero timers; start the pool on connect, join on disconnect; fan out only when queue depth > threshold (a count, not a timer).
- Memory: budget accounting must stay with the single store owner. Workers only compute; the cost (`msg_cost`) is returned with the message and the store charges it. Queues bounded by the same bytes cap (`queued_bytes`).
- Measured/expected gain: the pure part is 10-20% of 2-4 us, so little to fan out. Cross-thread handoff of a 100 B message costs about as much (about 0.5-1 us for channel + wake + cache miss) as the work. Not worth it for decode. Parallel payload formatting of one selected message is already on the background executor and one message is one job; splitting one JSON pretty-print across threads is not possible.
- Verdict: do not add a decode pool. A pool is only useful for filter/tree rebuild (R4) and only with snapshots.

### R6: store thread that exists only while connected
Fits with R3: spawn on connect, park on `recv`, exit on disconnect. A cheap idea alone gives nothing, because the store must also keep data when disconnected (browsing after disconnect). Keep it as one lifetime = the app session.

### R7: multi-thread net runtime
No. One TCP connection, one session, `mqtt-net` at 21% of one core at 75k msg/s. ntex types are not `Send` (ARCHITECTURE). More threads add contention and break ordering.

### R8: TLS/DNS/keyring/settings/log threads
Fine as is; off the UI thread, event-driven, parked. Nothing to change.

## 5. What NOT to do
- No parallel insert into the tree (counts to root, order maps, budget): lock contention, broken determinism and per-topic/global order.
- No hash-split workers for the message list without sequence numbers (the list is global arrival order).
- No second queue after the first without a byte bound; no polling worker pool (idle timers break AGENTS.md).
- No move of the merge off-UI through a `Mutex<TopicStore>` (UI reads would block on the lock and cost frames).
- Do not widen the net queue to hide drops: drops are store admission, and a bigger queue only adds memory and latency.

## 6. Is the current design good enough?
For normal brokers: yes. At 10k msg/s: merge p95 0.7-2.1 ms, main thread 6-9% CPU, zero drops with list, tree, filter, 100k topics. At 25k msg/s: zero drops over a 56 s run, merge p95 3.3 ms (at budget), main 9-14%. Frame budget: main-thread time per frame is about 4-6 ms (merge up to 3.4 + render preparation 1.5-2.5, and 3-4 at 100k topics): under 8.3 ms (120 Hz) and 16.7 ms (60 Hz) on a real GPU. Under Xvfb the 48 ms frame is llvmpipe, not our code. Real limit: about 25-30k msg/s sustained with 100 B payloads (less with large payloads: 25k at 1 KB drops); above that it drops by design (counted). Do R1 if more headroom is needed; R3 only for a clear 100k msg/s requirement.

Caveats: single runs; flood and Mosquitto share the 6 cores with the app (llvmpipe takes 1.5 cores); no perf call graph (kernel setting forbids), so the main-thread split between merge and render prep is derived from CPU ticks and metrics, not sampled stacks.

## Codex review of the analysis

The thread map is broadly correct, and I agree with R1 first. The analysis does not yet prove its throughput gains or responsiveness claims. These are the substantive issues.

1. **High — R1 can exceed the per-frame budget even when each slice is short.**  
   **Location:** analysis R1; [spawn_ingest](/home/pdeng/Workspace/mqtt-navigator/mqtt-navigator/crates/navigator-app/src/ui.rs:1411).  
   **Problem:** Throttling `notify()` while running more merge slices can put several slices into one display frame. Notification coalescing does not limit merge CPU time. Removing the timer can also leave the backlog loop without an effective yield. Render preparation and input handling share the main thread, so “frame rate stays the same because render is separate” is incorrect.  
   **Fix:** Allow one merge allocation per actual frame. Yield through a frame callback while backlog remains. Treat input latency, frame pacing and increased CPU use as acceptance criteria. R1 has moderate risk until measured.

2. **High — The existing 4 ms overruns need correction before increasing cadence.**  
   **Location:** analysis measurements, bottleneck 4 and R4; [merge](/home/pdeng/Workspace/mqtt-navigator/mqtt-navigator/crates/navigator-app/src/ui.rs:1539), [work](/home/pdeng/Workspace/mqtt-navigator/mqtt-navigator/crates/navigator-core/src/store.rs:1656).  
   **Problem:** The logs contain merge maxima of **6.59 ms** (`e25ktree.log`), **5.63 ms** (`i25k1k.log`) and **4.43 ms** (`h25k100kt-filter.log`). These are merge measurements, not frame measurements that can be dismissed as llvmpipe. When no maintenance exists at slice start, ingestion can consume 4 ms, create maintenance, then run a mandatory maintenance round. Also, the micro-test’s approximately 4 ms tree slice is an entire `work(now + 4 ms)` call, potentially containing many 256-node rounds. It does not establish a 4 ms single-round cost.  
   **Fix:** Reserve headroom even when maintenance starts during ingestion. Measure individual rounds before choosing smaller chunks or checks within chunks. Keep anchoring and other merge overhead inside the allocation.

3. **Medium — The logs cannot establish the stated division between queue and store drops.**  
   **Location:** analysis measurement table and bottlenecks 1–2; [merge counters](/home/pdeng/Workspace/mqtt-navigator/mqtt-navigator/crates/navigator-app/src/ui.rs:1547), [Ingest::queue](/home/pdeng/Workspace/mqtt-navigator/mqtt-navigator/crates/navigator-core/src/net.rs:1608).  
   **Problem:** `rate` counts generation-accepted dequeue attempts, including failed store inserts. Logged `drops` combines queue drops, store memory drops and topic drops. Thus “merged” does not mean admitted. The 75k run starts dropping at **97 MiB**, before the memory ceiling; the 1 KB run starts at **136 MiB**, also below that ceiling. Queue pressure matters too. Store admission pressure near 159 MiB is plausible, but its share is unmeasured.  
   The table also uses intermediate totals as final totals: the last logged counts are **282,547**, **1,141,579** and **304,092** for the 50k, 75k and 25k/1 KB runs.  
   **Fix:** Log separate counters for dequeued, admitted, stale-generation rejection, queue drops, memory-admission drops and topic drops. Include queue count, queued bytes and age. Report `list_drops` separately as eviction of previously admitted rows. Label the sampling interval and use actual end-of-log totals.

4. **Medium — CPU averages do not prove frame latency or the projected capacity.**  
   **Location:** analysis frame paragraph, bottleneck 1, R1/R3 gains and conclusion; [frame metric](/home/pdeng/Workspace/mqtt-navigator/mqtt-navigator/crates/navigator-app/src/ui.rs:3738).  
   **Problem:** CPU ticks are useful evidence of thread utilization. I also confirmed `perf_event_paranoid=4`. But CPU time divided by frame count is an average, not a latency bound. Subtracting merge p95 from that average cannot isolate render preparation. The metric measures elapsed time from render to a later frame callback; it does not isolate GPU time or presentation latency. Heavy llvmpipe activity makes this setup unsuitable for proving 60/120 Hz performance, but does not prove every tail comes from llvmpipe.  
   The 300k net capacity, 60–80k R1 capacity and 100–200k R3 capacity remain estimates. Low utilization at 75k supports headroom, not those ceilings.  
   **Fix:** Present the gains as hypotheses. Measure merge, maintenance, render preparation, frame intervals and input-to-visible-response latency separately on a real GPU.

5. **Medium — Render-side work and the filter comparison are understated.**  
   **Location:** analysis thread map, bottleneck 4 and R4; [live_rows](/home/pdeng/Workspace/mqtt-navigator/mqtt-navigator/crates/navigator-core/src/store.rs:1302), [render_tree](/home/pdeng/Workspace/mqtt-navigator/mqtt-navigator/crates/navigator-app/src/ui.rs:3197).  
   **Problem:** `rows()` describes tree rows. A dirty tree can rebuild its full expanded or filtered row vector synchronously during render, outside the merge timer. Preview construction also runs during render. It is bounded to a 512-byte payload head, but repeats for visible rows.  
   The scenario script filters for `zzneedle`, which hides the flood rows. Lower rendering cost can mask added filter cost. A collapsed tree with two rows does not exercise a large visible tree. Unchanged budget-limited merge p95 also does not prove unchanged insertion cost.  
   **Fix:** Measure row rebuilds and previews directly. Include matching filters, expanded trees, scrolling and open details. Keep background filter scans deferred, but address measured synchronous rebuild tails before proposing a pool.

6. **Medium — R1 needs explicit Pause and maintenance-progress checks.**  
   **Location:** analysis R1; [maintenance priority](/home/pdeng/Workspace/mqtt-navigator/mqtt-navigator/crates/navigator-core/src/store.rs:1656), [Pause semantics](/home/pdeng/Workspace/mqtt-navigator/mqtt-navigator/crates/navigator-core/src/store.rs:1684).  
   **Problem:** Pause freezes views while ingestion continues. Memory eviction precedes filter rebuilding and snapshot building. Persistent memory pressure can delay those jobs. Node eviction is disabled while paused, so new-topic pressure differs from ordinary streaming. Faster ingest can worsen these effects.  
   **Fix:** Verify bounded snapshot/filter completion under overload. Preserve paused counters, removed-row markers and frozen views. An empty incoming queue must still permit finite maintenance work; once both are empty, park on receive/kick with no periodic wake-up.

My ranked recommendation:

1. **Do R1 first, with corrected metrics and deadline handling.** Initial experiment: enter faster mode above **1,024 queued messages or 4 MiB**; leave below **256 messages and 1 MiB**. Use one allocation per display frame, initially capped at 60 allocations/s. Return to 33 ms coalescing below those thresholds. When fully drained, park.
2. Start with **2.5 ms ingestion**, a **3.25 ms maintenance deadline**, and **0.75 ms headroom**. Above the store budget, reduce ingestion to **1.5 ms** and give maintenance the remainder. Retain eviction’s existing low-watermark behavior. These are trial parameters, subject to measured round costs.
3. Reserve **R3** for a defined sustained requirement around **100k msg/s or more**, after R1 measurement. Keep one store owner, bounded communication and bounded snapshots. Do not promise its throughput yet.
4. Do not add a dynamic decode pool, topic-hash split or multithreaded net runtime now. GPUI already has a CPU-sized background executor. The measured evidence does not justify another pool and its ordering costs.

Prove R1 with repeated baseline/candidate runs on a real GPU. Warm the store to steady eviction, then test 25k/50k/75k rates with 100 B and 1 KB payloads. Record separate drop causes, queue age, per-frame merge time, frame intervals and input latency while typing, scrolling and toggling Pause. Require improved admitted throughput without worse input tails or merge work above 4 ms. After maintenance drains, verify no periodic ingest wake-ups.

No files were edited. I inspected the code and supplied logs; I did not run new load tests.
