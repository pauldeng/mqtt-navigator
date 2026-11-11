# Phase 7 verification (hardening)

Date: 2026-11-11. Reviewer for this phase: Cursor agent (grok-4.7-high).

## Done

- Soak test `crates/navigator-core/tests/soak.rs` (ignored by default) and a nightly CI job (1 hour, 500 msg/s, 6 broker restarts).
- Broker compatibility: EMQX 6.3.2 and HiveMQ CE 2026.5 in Docker beside Mosquitto; `broker_compat.rs` (CONNACK limits, TCP and WS round trips with MQTT 5 properties, retained delivery and clear, persistent session, restart reconnect).
- Security review of the whole repository (no critical or high findings) and fixes:
  - Topic names, filters, properties and broker reason texts show invisible and direction characters as `⟨U+XXXX⟩`; payload previews escape direction controls. Originals are kept for publish, subscribe and copy (`navigator-core/src/text.rs`).
  - Vendored client: an incoming QoS 2 PUBLISH or a reused packet id without DUP is a protocol error (DISCONNECT 0x82), not a panic; a DUP retransmission is delivered once (ADR 0002).
  - `--password` is removed; `--password-stdin` and `MQTT_NAVIGATOR_PASSWORD` replace it.
  - On Unix, the app's folders are 0700 and its settings, log and crash files are 0600 (`private_fs.rs`).
  - SECURITY.md matches the UI and the new protections.
- User docs: `docs/user-guide.md`, `CHANGELOG.md`, `docs/parity.md` (real status of each feature), README.
- Software renderer (Linux): the app starts with the Mesa lavapipe driver (first frame 238 ms).

## Checks run

| Check | Result |
|---|---|
| fmt, clippy (`-D warnings`), cargo-deny | pass |
| `cargo test --workspace` | pass: 271 tests (77 app, 130 core unit, 21 net, 41 update and launcher, 1 key check, 1 launcher process) |
| Vendored client tests | 323 pass |
| Broker matrix (Mosquitto) and compatibility (Mosquitto, EMQX, HiveMQ) | 15 + 15 pass |
| Soak 10 min (500 msg/s, 6 restarts) | after each restart: same generation, resubscribed, messages flow; 0 net, event and store drops; 1 881 messages lost only during the 1–3 s broker outages (clean session); RSS growth 17.7 MiB (limit 50, includes the history filling up); merge slice max 0.45 ms |

Broker differences (CONNACK): Receive Maximum EMQX 32, HiveMQ 10, Mosquitto 20; Maximum Packet Size EMQX 1 MiB, HiveMQ none, Mosquitto 2 000 000; none sent Server Keep Alive.

## Not checked

- The 1-hour soak and the manual 24-hour run (needs CI or a long local run).
- Windows and macOS builds, signing, installers, keyring, and WARP rendering (needs a push and CI).
- The UI has not been checked by a person (no screenshot or input automation here).

## Gate review 1 (Cursor agent, grok-4.7-high): findings and fixes

The reviewer confirmed the four security fixes on the named paths.

| Finding | Fix |
|---|---|
| The composer showed topics and properties from received messages raw | The keep-until-edit pattern also covers hidden characters: the field shows the escaped form and a note, and Send uses the original. Test: an RLO and ZWJ topic reaches the outgoing message unchanged. |
| The details topic field held the escaped text, so copy copied the escape | The topic is a label; "Copy topic" (button and palette) copies the original. |
| `--password=secret` was echoed in the error | Every `--password…` option except `--password-stdin` is refused without echo; unknown options print only their name. |
| Docs described behaviour the code does not have | User guide (no hover preview; tree has no activity dot or retained marker; reconnect rules), parity (tree: partial) and SECURITY.md (exact escaping scope) corrected. |

## Gate review 2: finding and fix

| Finding | Fix |
|---|---|
| Retry rules in the user guide did not match the code | The guide lists the cases that retry (connection refused, server unavailable, server busy, quota exceeded, connection rate exceeded) and the cases that do not. |
| The guide said encrypted client keys cannot be used, and mixed up the protocol-error text with the unsupported-version text | Corrected: enter the key passphrase; the protocol-error and MQTT 3.1.1 texts are described separately. |

Gate reviews 3-6 (docs only): the user guide was compared with the code until the reviewer reported no issues.
