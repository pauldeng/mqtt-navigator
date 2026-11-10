# ADR 0001: Technology stack

Status: accepted (2026-11-10).

## Decision

| Area | Choice |
|---|---|
| Language | Rust (edition 2024, latest stable toolchain, not pinned) |
| UI | `gpui-kit` 0.9.1, default features (`component`, `assets`). `tree-sitter` (JSON) is a possible later addition, decided in P5 by an ADR. |
| MQTT | `ms-mqtt-client` 0.1.0 with the `websockets` feature |
| Async runtime | tokio, on one dedicated `current_thread` runtime thread |
| TLS | OpenSSL through the client. The app enables `openssl/vendored`, so no system OpenSSL is necessary. |
| Secrets | `keyring` |
| Settings | `serde_json`, atomic write |
| Errors | `thiserror` (core), `anyhow` (app) |
| Logs | `tracing` + `tracing-subscriber` |
| Updater | Decided in P6 (ADR 0002) |

## Reasons

- The user selected gpui-kit and ms-mqtt-client.
- gpui renders on the GPU and starts fast. gpui-kit gives virtual lists, resizable panes, inputs, dialogs and themes.
- A dedicated tokio thread isolates network I/O from the UI executor.

## Consequences

- MQTT 3.1.1 is not supported. QoS 2 is not supported. The `Qos` type has no QoS 2 value, because the library can panic on QoS 2 paths.
- The library has no reconnect. We write our own supervisor.
- The library does not enforce Receive Maximum or Maximum Packet Size. We enforce outgoing limits. P1 checks incoming packet limits.
- We use the latest release of every dependency. `Cargo.lock` records the exact versions, and Dependabot proposes updates every week. gpui-kit pins `gpui-pre` exactly, so a gpui-kit upgrade also changes gpui. Do it in its own commit.
- The release profile keeps `panic = "unwind"`, so that a library panic stops one session and not the app.
