# MQTT Navigator

MQTT Navigator is a fast native desktop app to inspect MQTT traffic. It shows a live topic tree, message values and history. It supports MQTT 3.1.1 and MQTT 5. It publishes messages with MQTT 5 properties. It runs on Windows 10 or later, Ubuntu 24.04 and macOS 15 or later.

> Status: version 0.2.4 is released (MQTT 3.1.1 and MQTT 5). See [the changelog](CHANGELOG.md) and [the plan](docs/plans/2026-11-10-plan.md).

## Features

- **Subscribe.** Add and remove topic filters while connected. See messages in a live topic tree or in a newest-first message list.
- **MQTT 3.1.1 and MQTT 5.** Choose the Protocol in each profile. A new profile uses MQTT 3.1.1.
- **MQTT 5 subscription options.** Set No Local, Retain As Published and Retain Handling for each subscription.
- **Connection details.** A status-bar button shows the values from the broker CONNACK. A refused connection shows the server reference in the error text.
- **Publish.** Send messages with QoS 0 or 1, retain, and (MQTT 5) properties. See the broker acknowledgement.
- **Memory limit.** Set 32 MiB to 2 GiB. The default is 128 MiB.
- **Themes.** System, light or dark, with eight colour presets.
- **Auto-update.** Signed updates from GitHub Releases. You can turn them off.

## Key properties

- Native Rust app (gpui-kit). It starts fast and uses little CPU and memory.
- Memory has a fixed budget. A message flood cannot make the app grow without limit.
- TLS is on by default, with certificate checks. Passwords go into the system keyring.
- No telemetry.

## Limits

- QoS 0 and QoS 1 only. The app does not support QoS 2.
- MQTT 3.1.1 has no message properties, no reason codes and no subscription options. A message can be at most 1 MiB.
- Keep alive 0 turns off the detection of a silent broker, unless an MQTT 5 broker sets its own keep alive.
- After a very high message rate, the app can keep more memory than at a normal rate until it exits. The memory stays bounded.
- The macOS app needs macOS 15 or later.
- Windows and macOS files are not signed with a trusted publisher certificate or notarized. The first start needs one extra step. See [the install steps](docs/user-guide.md#install).
- Version 0.1.x cannot read the settings of 0.2.0. See [the user guide](docs/user-guide.md#go-back-to-version-01x).

## Install

Download the installer or package for your system from [GitHub Releases](https://github.com/pauldeng/mqtt-navigator/releases). Read the [user guide](docs/user-guide.md) for the steps on Windows, macOS and Linux.

## Build from source

Requirements: the latest stable Rust (`rustup update stable` or `mise install`). On Linux, install the system packages in [AGENTS.md](AGENTS.md#system-packages).

```bash
cargo run --release
```

To build the installers, see [docs/release.md](docs/release.md).

## More information

- [docs/user-guide.md](docs/user-guide.md): install and use the app.
- [ARCHITECTURE.md](ARCHITECTURE.md): how the app works.
- [AGENTS.md](AGENTS.md): commands and engineering rules.
- [SECURITY.md](SECURITY.md): how to report a security problem.
- [docs/](docs/): decisions, plans and the feature checklist.

## License

Use MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
