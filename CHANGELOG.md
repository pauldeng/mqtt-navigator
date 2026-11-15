# Changelog

All notable changes to this project are in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). The project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- MQTT 3.1.1 support. The **Protocol** setting in the connection sheet selects MQTT 3.1.1 or MQTT 5. A new connection uses MQTT 3.1.1. The command line has `--mqtt 3` (default) and `--mqtt 5`.
- MQTT 3.1.1 connections show Clean session, no properties, and plain text for CONNACK return codes and SUBACK failures. A message can be at most 1 MiB.
- Plain errors when the Protocol does not match the broker.
- MQTT 5: subscription options (No Local, Retain As Published, Retain Handling) for each subscription.
- A **Connection details** button in the status bar. It shows values from the CONNACK (MQTT 5: session present, keep alive in use, the broker limits and features, and the optional texts; MQTT 3.1.1: session present). See the user guide for the list.
- A refused connection or a DISCONNECT shows the server reference in the error text. The app does not follow it.

### Changed

- New MQTT client library: `ntex-mqtt` replaces `ms-mqtt-client`. The local patch and the vendored copy are gone ([ADR 0006](docs/adr/0006-ntex-mqtt.md)).
- Settings schema is 2. A connection saved by 0.1.x has no Protocol and loads as MQTT 5. Version 0.1.x does not load a schema 2 file: it leaves the file unchanged, uses default settings and does not save.
- The app finds a connection that does not answer in about 2 times the keep alive, plus timer and shutdown delays (0.1.x: keep alive plus 10 s).
- The connection ends with new plain texts when the broker stops reading, or stops in the middle of a packet.
- The release file is 2.2% larger than 0.1.1 (35 699 608 B against 34 943 904 B on Linux).

### Known limitations

- QoS 2 is not supported.
- Keep alive 0 turns off the detection of a silent broker, unless an MQTT 5 broker sets its own keep alive.
- After a very high message rate, the app can keep more memory than at a normal rate until it exits. In a Linux test it stayed the same over repeated overloads. Version 0.1.1 does the same.
- If you go back to 0.1.x, it does not load the 0.2.0 settings file. It leaves the file unchanged, uses default settings and does not save.
- The macOS app needs macOS 15 or later.

## [0.1.1] - 2026-11-12

### Changed

- One program on every platform: `mqtt-navigator` (`mqtt-navigator.exe` on Windows). The separate launcher is gone. The app applies a staged update at its own start and rolls back a version that never opens a window.
- Windows: the exe has no console window. In a terminal, `--help` and errors still print.
- A Windows install of 0.1.0 keeps `mqtt-navigator-launcher.exe` as the shortcut target. It still works: it starts `mqtt-navigator.exe`. To remove it, uninstall the app, then install the new version.
- A development build never updates itself.
- macOS: one universal app (Apple silicon and Intel) in one `.dmg`, as `…-universal-apple-darwin`.
- Windows: the exe links the C runtime statically. It does not need the Visual C++ Redistributable.

## [0.1.0] - 2026-11-12

First version.

### Added

**Connections**
- Connection profiles with TLS (default, port 8883), WSS, TCP and WS.
- TLS certificate checks on by default. Custom CA file and client certificate (mutual TLS).
- Plain-language TLS errors. A "Not encrypted" tag for TCP and WS. An "Insecure TLS" tag when verification is off.
- Passwords and key passphrases in the system keyring. A session-only fallback with a notice.
- Clean start, session expiry, keepalive and Will message.
- Automatic reconnect with backoff, and resubscribe. Subscriptions removed while offline are also removed on a resumed session.
- Profile import and export (JSON, no secrets).
- Command line options for a session connection.

**Subscribe**
- Add and remove topic filters while connected, with QoS 0 or 1.
- The broker result for each filter, with Retry and Remove.
- A colour for each subscription, and "Show only" to filter the views.

**View**
- A newest-first Messages view and a live topic tree.
- Filters: text, subscription, All / Received / Sent.
- Pause and Follow. Row height of 1 or 2 lines.
- Details with decoders (Auto, Text, JSON, Hex, Base64, Number), MQTT 5 properties and per-topic history of 100 messages.
- Background formatting of long payloads, with Show all and Show more.

**Publish**
- Always-visible composer: Text or Hex payload, QoS 0 or 1, retain, MQTT 5 properties (content type, response topic, correlation data, message expiry, payload format, user properties), JSON format.
- Drafts and Sent history. Edit and publish, Reply, Publish to this topic, Clear retained.

**Memory and speed**
- One memory limit for all stored messages: 32 MiB to 2 GiB, default 128 MiB.
- Bounded queues, a drop counter and a limit of 1 MiB for incoming packets.
- No work while the app is idle. No telemetry.

**Appearance**
- System, Light and Dark appearance. Eight theme presets.
- Command palette (`Ctrl/Cmd+K`) with most actions.
- Saved window size, pane widths and view.

**Updates and packages**
- Signed updates from GitHub Releases. One check at start, which you can turn off.
- Rollback to the previous version when a new version does not open its window.
- Windows installer and portable zip, macOS DMG (arm64 and x64), Linux AppImage and deb.
- The app icon on every platform. Windows: the exe has the icon resource. Linux X11: the window has an icon. Linux Wayland: the `.deb` has a desktop entry and icons named after the app id. The AppImage and the bare program add a user entry and icons on the first start.
- SBOM, SHA-256 sums and build attestations for each release.

**Support**
- "Open the log folder" and "Copy diagnostics" (no secrets).
- The [user guide](docs/user-guide.md).

### Known limitations

- MQTT 5.0 only. QoS 0 and 1 only.
- No proxy support, no message export, no accessibility support.
- The Windows files have no Authenticode signature. SmartScreen warns on the first start.
- The macOS app has an ad-hoc signature, not a Developer ID. It is not notarized. The first start needs right-click, then **Open**.
- A deb install does not update itself.
- CI builds and tests Windows and macOS. A person has not yet run the Windows installer, the macOS DMG, the keyring on those systems, or the update swap.
- A person has not yet checked the UI in a real window.
- The `ms-mqtt-client` library has no limit for the packet size. The app uses a local patch.
- Software rendering is only partly tested. On Linux the app starts with the Mesa lavapipe driver. Windows WARP, virtual machines and remote desktops are not tested.

[Unreleased]: https://github.com/pauldeng/mqtt-navigator/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/pauldeng/mqtt-navigator/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/pauldeng/mqtt-navigator/releases/tag/v0.1.0
