# Changelog

All notable changes to this project are in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). The project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
