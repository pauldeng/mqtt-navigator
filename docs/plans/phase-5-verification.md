# Phase 5 verification (inspector and full publish)

Date: 2026-11-11.

## Advisor decisions (Codex)

- D1: pure core decoders (`decode`: Text, JSON with `serde_json` and depth/output caps, Hex dump, Base64, numbers 8–64 bit and f32/f64 in LE/BE); `auto` never guesses numbers; 256 KiB first, "Show all" up to the stored bytes; one background job with generation and cancel; cargo-fuzz on nightly CI, fixed-corpus tests on stable.
- D2: plain gpui-kit `Editor`; `tree-sitter` JSON highlight only with an ADR, ≤ 3 MB and unchanged budgets.
- D3: composer with exact bytes, all MQTT 5 properties, Drafts (5) and Sent (20) with byte caps, core validation (`publish::validate_outgoing`) before send.
- D4: Edit and publish copies exact bytes and is disabled for a truncated or evicted payload; Reply uses Response Topic and correlation data; Clear retained publishes an empty retained payload at QoS 1 after a confirm.
- D5: one action registry for the palette (Ctrl/Cmd+K) and the key bindings.
- D6: Open log folder; Copy diagnostics (version, OS, architecture, GPU adapter; no profiles, hosts, paths, topics, payloads or secrets).
- D7: measure four release profiles and select the smallest one that passes the budgets; CI size gate = measured × 1.10, Linux first.

## Done

- Core: `decode.rs`, `publish.rs` (validation, encoded size, hex parse/format), `Net::send` validates a Publish, `Net::broker_max_packet()`; `fuzz/` crate with four targets; CI `fuzz` job (weekly and on push to main).
- App: inspector (Value and History tabs, decoder Select, background formatting with cancel, Show all, Wrap, Expand/Restore, MQTT 5 properties), composer (Text/Hex, properties, Format, collapse, History ▾, validation), actions (Publish to this topic, Edit and publish, Reply, Clear retained), narrow-window details sheet, command palette, diagnostics.
- Dev profile: no debug info for third-party crates (`target/debug` 15 GB → 2.8 GB).

## Checks run

| Check | Result |
|---|---|
| fmt, clippy (`--all-features`, `-D warnings`), cargo-deny | pass |
| `cargo test --workspace` | pass: 175 tests (55 app, 99 core unit, 21 Mosquitto and fake-broker) |
| Fuzz (local, nightly 1.102, 20 s each) | no crash: decode_render 885k, parse_hex 2.9M, topic_matches 1.55M, profile_import 1.64M runs |
| App flood, 10k msg/s, 10k topics, 60 s | 0 drops; RSS 244 MiB; store 127 MiB; merge p95 median 0.84 ms (max 2.0 ms); frame p95 median 6.7 ms; worst frame under load 18.9 ms (the 90 ms frame is at start-up, before traffic) |
| First frame (warm cache, 5 runs) | 198–348 ms (one cold run after a rebuild: 1 080 ms) |

## Not checked

- No UI interaction in a real window: tabs, decoder Select, composer keys (Tab, Ctrl/Cmd+Enter), History ▾, palette confirm, narrow sheet, Clear retained dialog focus, layout.

## D7: release profile and size gate

See ADR 0004. Selected `opt-level = "s"`, `lto = "fat"`: 33 924 696 B (−29% against 47 702 232 B with opt-level 3 and thin LTO), frame p95 median 7.0 ms, worst frame 8.5 ms, worst merge 3.1 ms, 0 drops, first frame about 210 ms. The CI `size` job enforces 37 317 165 B for Linux x64.

## Codex P5 review fixes

- The editor takes a 4 KiB first window. The text then grows in timed steps (6 ms target, step size from the last step) to 32 KiB. "Show more" adds 64 KiB in the same way. Copy copies all of the formatted text, and only when the render is the one of the selection. The composer Format refuses a payload above 1 MiB or a result above 2 MiB.
- Measured, release, 360 px pane, trickle of 20 msg/s (`MQTT_NAVIGATOR_METRICS=1`, `MQTT_NAVIGATOR_AUTOSELECT=<topic>`, which also connects and applies Show all; for measurement only). 1.0 MB JSON: worst frame at selection 23.8 ms, steps 2 to 5 ms, later frames at most 17.6 ms. 2 MB hex (stored 1 MiB, 5 MB of text): worst frame at selection 58 ms (cold glyph cache in the first window), steps 5 to 7 ms, later frames at most 18.7 ms. Before the change, one `set_value` of the whole text put 614 ms into one frame (1.9 MB JSON). The editor wraps a hex dump at 3 to 4 ms for each KiB, so hex is the limit.
- A broker message above 2 MiB (`NetLimits::max_packet`) is not received, so a 2 MB payload was published as 2 000 000 bytes.
- Retained state is tracked for each topic in the store (see `TopicStore::has_retained`).
- Not checked: no UI interaction (History keys, "Send empty", escaped line breaks, palette entries) in a real window.

## Codex review 2: findings and fixes

Verdicts: the selection stalls (23.8 ms for 1 MB JSON, 58 ms cold for 2 MB hex) are acceptable for P5; `MQTT_NAVIGATOR_AUTOSELECT` (needs `MQTT_NAVIGATOR_METRICS=1` too) is acceptable in the shipped binary.

| Finding | Fix |
|---|---|
| A growth target inside a multi-byte character stopped the growth forever (timers kept running) | `inspector::next_cut` always advances to a character boundary; `grow_step` stops when no progress is possible. Regression tests for automatic growth and Show more. |
| A retained clear did not finish when its list row was evicted before the PUBACK | `pending_clears` (at most 64) is independent of the list rows. Test: eviction before the acknowledgement. |

Tests: 187 (62 app, 104 core unit, 21 Mosquitto and fake-broker).

## Codex review 3: finding and fix

| Finding | Fix |
|---|---|
| The inspector payload Editor and topic Input were editable (the builder default overrides `set_readonly(true)`) | Both use `.readonly(true)` in render. |

## Codex review 4: findings and fixes

| Finding | Fix |
|---|---|
| Clear retained was missing after our own retained publish to a topic without a tree node | Our retained publishes are tracked (`pending_retained`, at most 64) and, on success, kept in a topic-keyed map (at most 1 024) with the message expiry. `has_retained_topic` uses the node state or the map. Test without a node. |
| A history selection inherited "Sent" from the previous selection | History selections use `Direction::Received`. |

## Codex review 5: finding and fix

| Finding | Fix |
|---|---|
| A sent row's History said "no history" after the broker echo created the node | `shown()` keeps the node it resolved by topic. |

## Review 6 (Cursor agent, grok-4.7-high; Codex had no credits): findings and fixes

| Finding | Fix |
|---|---|
| High: the MQTT 5 property list used three columns in the narrow pane, so values were clipped | `DescriptionList::columns(1)`. |
| An incoming retained message did not replace our own retained record, so an expired broker value kept Clear retained available | Every incoming retained message removes the topic from `sent_retained`; the node is the record. Test: `a_broker_retained_message_replaces_our_own_record`. |
