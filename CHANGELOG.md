# Changelog

All notable changes to this project are in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). The project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Decoders **Base64URL**, **Base64 → Hex** and **Base64URL → Hex**. Base64URL shows the bytes as URL-safe Base64 text without padding. The two Hex decoders read the payload as Base64 or Base64URL text, with or without padding, and show the bytes as hex. White space in the text is ignored. Auto never picks them.

### Changed

- The details pane shows a **Topic** caption before the topic. The **Value** tab is now **Message**.
- The Hex decoder shows only the bytes as hex pairs, like `00 ff 10`. The offset and ASCII columns are gone. You can copy the text into the composer in Hex mode.

### Fixed

- You can remove a subscription while the app is not connected. The row goes at once, and the next connect does not subscribe it.

## [0.2.7] - 2026-11-26

### Changed

- The release build is optimized for speed, not for size (`opt-level = 3`). The UI uses 6 to 11% less CPU time for each frame under heavy traffic. The binary is about 33% larger. See ADR 0004.
- The composer and the details pane are separate views. Most new messages do not redraw them any more. With the composer open (MQTT 5, 20 user properties) under 10k messages a second, the UI CPU time for each frame fell from about 9.4 ms to 5.3 ms in our measurement (Linux, software rendering).
- The store shares the MQTT 5 properties of a message between the latest value, the history and the message list. A new message is stored faster.
- When one merge handles 256 or more messages, the message list redraws less often (about 15 times a second on Linux, not 30). This is about 7.7k messages a second (about 10k on Windows). Backlog mode does not do this. The app still merges all messages at the same pace. This lowers the UI CPU time. The last messages of a flood show about 50 ms after the last merge.

### Fixed

- The subscription field shows a clear error for a topic filter with a control character, or with an incorrect shared subscription group (`$share/...`).
- Topics, filters and property text show more invisible format characters as a visible code (for example `⟨U+0890⟩`).

## [0.2.6] - 2026-11-25

### Changed

- The app uses gpui-kit 0.11.0. The value pane of the details shows the current line only while it has the focus.
- The composer has a maximum height. In a narrow centre pane, each property row uses two lines. Only the user property rows scroll. The Name and Value captions stay in view.
- The message list and the tree take the focus with Tab. The command palette shows the shortcut of each command.
- Tab is faster in a composer with many user properties. The composer builds only the rows near the visible area.
- On Windows, the app waits less between two updates of the message list under heavy traffic (25 ms instead of 33 ms). Before, the window updated only 21 to 23 times a second. The new value is not measured on Windows yet.
- Under very heavy traffic, the tree filter shows its result sooner (before: up to 1 s).

### Fixed

- A JSON payload larger than 256 KiB showed an empty value. The value now shows the start of the payload as plain text. Show more shows more of it, up to 256 KiB.
- On a topic with fast, large messages, the value could stay at "Formatting…", and Show more and Show all were not available.
- App shortcuts (for example `Ctrl+Shift+N`) did not work right after the app started.
- The focus went back to the correct control after a menu, the connection sheet or the History dialog closed. Before, the focus could go to no control or to the wrong control.
- After a disconnect, the message list said that a sent message was not sent. The composer hid the last publish result.
- The subscriptions could show old data after a change of the connection profile.
- History and the value could show the payload of a different message with the same topic, time and size.
- The command palette could run a command that no longer applied, for example Disconnect after the broker closed the connection.
- The connection sheet works with the keyboard: arrow keys in the saved list, and Enter in a field runs Save and connect. A validation error moves the focus to its field. TCP and WS do not check the client certificate and key. A new password or passphrase cancels the removal of the saved secret.
- The message list, the tree, the subscriptions and History have scrollbars. A drag of the message list scrollbar away from the top turns off Follow.
- At a high zoom, the details open in a sheet when the three panes do not fit.
- Borders and the status bar had layout errors in the main window.
- The `url` dependency had a security advisory (RUSTSEC-2026-0592).

## [0.2.5] - 2026-11-20

### Changed

- The composer has a visible ✕ button to close it, at the end of the topic row. `Esc` also closes it.
- The composer menu has **Beautify JSON** and **Minify JSON**. They replace **Format**.
- The composer has a **JSON** payload mode next to Text and Hex. Valid JSON gets syntax colours. Invalid JSON shows a warning, and you can still publish it.
- The composer button is **Publish** (was Send).

### Fixed

- On Windows, console windows flashed open and closed when the app unpacked a downloaded update. The archive tool now runs without a window.
- The payload text of the composer had a left indent. It now starts at the same edge as the topic.

## [0.2.4] - 2026-11-20

### Changed

- The tree is the default view of a new install. An install that saved a view keeps it. A click on a topic selects it. A double click expands or collapses a topic that has subtopics. A click on such a topic does not open the details: press Enter or click Details.

### Fixed

- On Windows, the buttons in the title bar did not work: for example Connect, Disconnect and "Add a connection". Windows moved the window and took the click.

## [0.2.3] - 2026-11-19

### Added

- The settings menu shows the app version on its last line, for example "Version: 0.2.3".

### Fixed

- The update check could show "The update signature is not valid." for a few minutes after a new release. GitHub sent the manifest and its signature from two different releases. The app now resolves the address of the latest release once and reads both files from it.

### Changed

- Dependency updates, including gpui-kit 0.10.0 and ntex-mqtt 10.0.2. ntex-mqtt has no exact pin now.

### Known limitations

- Versions 0.2.2 and earlier keep the old check. For a few minutes after a new release, their update check can fail with this text. A later check works.

## [0.2.2] - 2026-11-19

### Added

- A connection menu in the title bar replaces the connection list. The menu button shows the active connection with its host and port, for example `lab-broker · localhost:18830`. The menu lists every connection with its address. A check shows the active one. The menu also has **Edit "name"…**, **New connection… (Ctrl/Cmd+N)** and **Manage connections…**.
- When a connection is active and disconnected or failed, and the message list is empty with no filter, the centre shows a card: the name, the URL, the protocol and the encryption, with **Connect** and **Edit connection…**. A failed connection also shows the reason and **Try again**.
- A failure banner above the message list when a connection fails while messages are on screen. It has **Try again**, **Edit connection…** and **×**.
- The tooltip of the **Connect** button names the connection and its URL.

### Changed

- With no active connection, the title bar button says "Add a connection" (none exists) or "Choose a connection". The **Connect** button does not show then. Before, the list said "No connections" even when connections were saved.
- When connections exist but none is active, the centre shows **Choose a connection ▾** (the same rows as the title bar menu) and **New connection…**.
- **Save** of a new connection makes it active when the state is Disconnected or Failed and no connect is pending. The app clears the data of the old connection. Before, the app saved the new connection but did not select it.
- The connection sheet title is "New connection" or "Edit name". Before, it was "Connections" for both.
- While the app disconnects, the button says "Disconnecting…" and the centre shows "Disconnecting from name…". Before, the centre showed a **Connect** button that did nothing.
- The settings menu puts Appearance, Theme, Row height and Memory budget in submenus. The top level is short.
- An IPv6 host shows in brackets, for example `[fd00::1]:1883`.
- Keyboard focus: after you choose a connection or select **Save**, the focus goes to the primary button. When the "Disconnect from … and connect to …?" question closes, the focus is on the connection menu button.

### Fixed

- In a small window, the settings menu jumped by two rows one frame after it opened (seen on Windows). The top level now has no checked item and does not scroll, so **Commands…** stays the first line.

### Known limitations

- You cannot edit the "Command line" connection (from command line arguments). The menu and the card show "Command line: not saved".

## [0.2.1] - 2026-11-18

### Added

- A settings menu (the gear in the title bar): Commands…, Appearance, Theme, Row height, Memory budget and Reset the layout.
- The last line of the settings menu shows the renderer: GPU or software, and the drawing API. "Copy diagnostics" also names the API.
- A **Details** popover in the status bar. It holds the parts that the status bar no longer shows.
- **Follow** keeps the newest message in view. It has no switch. When the list leaves the top, Follow stops and a **Jump to newest (N)** button shows.
- **New message** (`Ctrl/Cmd+Shift+N`) opens the publish composer.
- Subscriptions: each row has a menu with Show only, QoS, Options, Retry and Remove. The add field has a QoS menu. A QoS change sends a new SUBSCRIBE. The command palette has the subscription actions too, and it shows a reason when an action cannot run.
- Each active Show only or direction filter is a tag under the header, with its own **×** button.
- Empty states for a connection with no subscriptions and for a broker that rejects `#`.
- The details show the result of a sent message.
- The metrics log line counts each drop cause (net queue, store memory, topic limit, list rows) and shows the net queue length, size and age.

### Changed

- A simpler UI:
  - The title bar has the profile, the state, the settings menu and one main action. The filter moves to the centre header.
  - The status bar shows a part only when it applies.
  - A subscription row has a status dot, the filter, a Q1 tag and a menu.
  - The centre header has one row: the view switch, the filter, a direction menu and Pause.
  - A message row has the topic, R and the time on the first line, and the preview on the second line. A row shows a send result only while it is pending or when it failed. It no longer shows "Sent" or "Acknowledged".
  - The publish composer is closed at start. When open, it shows Text or Hex, QoS, Retain, a menu and Send. Properties show only when present, with an **Add property** menu. Closing the composer keeps the draft. **Send** in the command palette opens a closed composer first, so it never sends a draft that you cannot see. `Esc` closes the focused composer or details pane and moves the focus to the list.
  - In a wide window, the details open when you select a message or a topic. In a narrow window, `Enter` opens the details sheet.
  - The details header has the topic, which you can select, and a menu. **Edit and publish** is the one action button. Properties are collapsed. When you choose a History entry, the Value tab shows a note and a **Show latest** button.
  - The connection sheet moves the client ID to Advanced, and the client certificate, key and passphrase to a collapsed section.
  - The security tag is easier to read.
- Faster ingest under a large backlog: while the net queue is deep, the app merges once for each display frame, not each 33 ms. We tested it on a software renderer (22 frames/s) with 100-byte messages:
  - At 25 000 msg/s and less, the app accepted the same number of messages.
  - At 50 000 msg/s, the app accepted 24 % more messages. At 75 000 msg/s, it accepted 121 % more.
  - With a text filter at 50 000 msg/s, it accepted 9 % more.
  - With an expanded tree at 50 000 msg/s, it accepted 19 % fewer. One of the two runs gives most of this drop. We did not find its cause.
  - Under overload, messages wait in the net queue (up to about 1.5 s). Without a filter or an expanded tree, the net queue drops the excess. With a filter or an expanded tree, the store can also drop messages.
- The store does its clean-up in smaller rounds (32 items, before 256), so one round stays in the frame budget.

### Fixed

- A TLS or WSS connection with no CA file now trusts the CA certificates of the operating system. Before, a broker with a valid public certificate (for example Let's Encrypt) showed "The server certificate is not trusted." The app loads the system CA certificates once for each run. If it finds none, the error text says so.
- A CA file now replaces the system CA certificates. The app trusts only that file. Before, the user guide said that the app trusts both.

### Known limitations

- Under heavy load, a merge can take more than the 4 ms frame budget (up to about 6 ms, and once 8.8 ms). We measured this on a software renderer. We did not measure it on a GPU.
- On macOS, "Copy diagnostics" does not name the graphics adapter.

## [0.2.0] - 2026-11-15

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

### Fixed

- A TLS or WSS connection with no client certificate that the broker closes during set-up now says "The broker closed the connection during set-up. If the broker needs a client certificate, add one to the connection." The broker can reset the connection and drop the TLS alert (always on Windows). Before, the text said only that the broker closed the connection.

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

[Unreleased]: https://github.com/pauldeng/mqtt-navigator/compare/v0.2.7...HEAD
[0.2.7]: https://github.com/pauldeng/mqtt-navigator/compare/v0.2.6...v0.2.7
[0.2.6]: https://github.com/pauldeng/mqtt-navigator/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/pauldeng/mqtt-navigator/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/pauldeng/mqtt-navigator/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/pauldeng/mqtt-navigator/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/pauldeng/mqtt-navigator/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/pauldeng/mqtt-navigator/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/pauldeng/mqtt-navigator/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/pauldeng/mqtt-navigator/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/pauldeng/mqtt-navigator/releases/tag/v0.1.0
