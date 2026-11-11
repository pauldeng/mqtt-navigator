# MQTT Navigator

MQTT Navigator is a fast native desktop app to inspect MQTT traffic. It shows a live topic tree, message values and history. It publishes messages with MQTT 5 properties. It runs on Windows, Ubuntu and macOS.

> Status: early development (phase P0). There is no release yet. See [the plan](docs/plans/2026-11-10-plan.md).

## Features

- **Subscribe.** Add and remove topic filters while connected. See messages in a live topic tree or in a newest-first message list.
- **Publish.** Send messages with QoS 0 or 1, retain, and MQTT 5 properties. See the broker acknowledgement.

## Key properties

- Native Rust app (gpui-kit). It starts fast and uses little CPU and memory.
- Memory has a fixed budget. A message flood cannot make the app grow without limit.
- TLS is on by default, with certificate checks. Passwords go into the system keyring.
- No telemetry.

## Limits

- MQTT 5.0 only. MQTT 3.1.1 brokers cannot connect.
- QoS 0 and QoS 1 only.

These limits come from the MQTT library ([ms-mqtt-client](https://github.com/microsoft/rust-mqtt-client)).

## Build from source

Requirements: the latest stable Rust (`rustup update stable` or `mise install`). On Linux, install the system packages in [AGENTS.md](AGENTS.md#system-packages).

```bash
cargo run --release
```

To build the installers, see [docs/release.md](docs/release.md).

## More information

- [ARCHITECTURE.md](ARCHITECTURE.md): how the app works.
- [AGENTS.md](AGENTS.md): commands and engineering rules.
- [SECURITY.md](SECURITY.md): how to report a security problem.
- [docs/](docs/): decisions, plans and the feature checklist.

## License

Use MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
