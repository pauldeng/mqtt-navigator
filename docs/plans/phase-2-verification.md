# Phase 2 verification (session robustness)

Date: 2026-11-11. Machine: Ubuntu (GNOME, Wayland). Docker broker: `eclipse-mosquitto:2.1.2-alpine` (pinned by digest).

## Advisor decisions (Codex)

- D1: retry temporary faults (transport loss, refused, timeouts, ping timeout, temporary DNS failure, "server unavailable / busy / shutting down / rate" reason codes) with backoff from 1 s to 30 s and ±20% jitter. The counter resets after 30 s of a stable connection. Stop as Failed for credentials, authorization, bans, protocol errors, TLS certificate errors, packet too large, our operation timeout, a library panic, bad configuration, and unknown errors. The generation changes only on a user Connect. Resubscribe the current desired filters after each connect.
- D2: `tracing` with our own size-capped file writer (5 MiB, one `.1` file, bounded channel, background thread). Default: app INFO, dependencies WARN, `ms_mqtt_client` off. No payloads or secrets in logs.
- D3: Mosquitto in Docker with six listeners, certificates made at test time, the `broker-tests` feature, and a separate CI job. EMQX and HiveMQ move to P7.
- D4: TLS diagnostics through `set_verify_callback`, which records the first failing X509 code.
- D5: the live Subscriptions panel, SUBACK reasons and recovery states stay in P2. The "Reconnected" row waits for the P3 message list.

## Done

- `navigator-core/src/net.rs`: reconnect supervisor with the D1 policy; attempt tokens (a stale or cancelled attempt cannot set the state or queue messages); desired subscription list that follows live Subscribe and Unsubscribe, also while disconnected; `Event::Reconnected`; panic in an operation task is contained; `tracing` calls without payloads or secrets. A TLS handshake that the broker cuts off is retried.
- `navigator-core/src/tls.rs`: own OpenSSL connector (TLS 1.2+, system roots + extra CA, client certificate with a key check), verify callback, plain texts for expired, not yet valid, not trusted, wrong host name, and key mismatch.
- `navigator-app`: title bar "Reconnecting (n)…" with the reason, and Cancel; live Subscriptions panel (Enter subscribes, QoS 0/1, filter validation, duplicate check, Retry and Remove, "Remove #" hint, "Waiting for connection"); data stays across reconnects; `logging.rs` (size-capped writer, panic hook writes a crash file).
- `tests/broker/`: Compose file, Mosquitto configuration, certificate script (good, expired, wrong host, mismatched key), `up.sh` with a real CONNACK readiness check, `down.sh`, README.
- CI: new `broker-tests` job on Ubuntu.

## Checks run

| Check | Result |
|---|---|
| `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings` | pass |
| `cargo test --workspace` (parallel, and `--test-threads=1`) | pass: 64 tests (13 app, 40 core unit, 11 Mosquitto and fake-broker) |
| Broker matrix (`tests/broker/up.sh`, `cargo test -p navigator-core --features broker-tests --test broker_matrix -- --test-threads=1`) | pass: 15 tests |
| `cargo deny check` | pass |

Broker matrix coverage: QoS 0 and 1 round trips on anonymous TCP, password TCP, TLS, mTLS, WS and WSS; wrong password (Failed, no retry); TLS without the CA (not trusted), expired certificate, wrong host name; verify off; mTLS without a client certificate, and with a mismatched key; broker restart (Reconnecting, then Connected, resubscribe works, same generation); no delivery after Disconnect.

## Not checked

- Windows and macOS builds and the CI jobs: nothing is pushed yet.
- The Subscriptions panel and the title-bar states were not looked at on screen. They are covered by the build and the unit tests only.
- "A stable connection of 30 s resets the counter" is tested as a pure function only.
- `nothing_is_queued_after_disconnect` does not fail when the gate is removed, because the timing window is too small. `stale_attempt_results_are_dropped` proves the gate.
- A subscription that the broker rejects stays in the desired list. It is sent again after each reconnect, until the user removes it.

## Codex review 1: findings and fixes

| Finding | Fix |
|---|---|
| More than 256 filters: resubscribe after a reconnect dropped the rest | Desired filters go out in batches, while fewer than half of the operation slots are in use. Test: 356 filters, all subscribed, at most 256 open. |
| Remove hid the row before the UNSUBSCRIBE was sent or acknowledged; Add made a row even when the send failed | Rows have states: Pending, Active, Failed, Removing, Remove failed. A row goes away only on `Unsubscribed`. A failed send keeps the row (remove) or makes no row (add), and shows the error. |
| An encrypted client key could make OpenSSL prompt on the terminal and block the net thread | An encrypted key fails at once with a plain text. Parsing uses a callback that never prompts. |
| The panic hook writes a file on the panicking thread | Not changed, by decision: a panic ends the app, and a background write would often be lost. AGENTS.md records this exception. |
| The Subscriptions list could not scroll | The rows are in a scroll region. The input stays below it. |
| A missing client certificate was reported as a server certificate problem | A peer alert about client authentication gives "The broker needs a client certificate." The matrix test checks the text. |

## Codex review 2: findings and fixes

| Finding | Fix |
|---|---|
| A removal that the net thread already applied could come back on the next manual Connect | A row whose removal the net thread accepted (Removing, Remove failed) is not in the Connect profile, and it goes away on a connection change. A removal that could not be queued is a separate state (Not removed) and stays wanted. Test: remove, connection loss before UNSUBACK, Connect. |
| An unsubscribe rejection had no filter and could mark the wrong row | `Event::Unsubscribed` now carries `filter` and `result: Result<(), String>`. Each result goes to its own row. Test: answers in reverse order. |
| Panic hook (verdict) | Accepted. The AGENTS.md wording now also covers contained panics. |
