# Feature checklist

This list compares MQTT Navigator with the behaviour of mqtt-pathfinder. It also lists improvements. Update the status when a phase finishes.

Status: `done`, `dropped` (with a reason), `better` (we do more than the reference). `todo` means not started.

"Done" means the code and the tests pass. Nobody has checked the UI by hand yet (see [open questions](open-questions.md)).

| Feature | Reference behaviour | Our implementation | Phase | Status |
|---|---|---|---|---|
| Connection profiles | name, host, port, mqtt/ws, TLS toggle, client ID | Same, plus TCP/TLS/WS/WSS selector; "Save and connect" tests the profile; session and Will under Advanced | P4 | better |
| MQTT version | 3.1.1 and 5 | 5 only (library limit) | — | dropped |
| QoS | 0, 1, 2 | 0, 1 (library limit) | — | dropped |
| TLS | separate toggle, off by default; verify **off** by default | TLS **on** by default (port 8883); verify **on** by default; "Not encrypted" warning for TCP/WS; plain-language TLS errors; mTLS | P2 | better |
| Secrets | OS secure storage, session fallback | System keyring, session fallback with a notice. Not run on a real keyring yet. | P4 | done |
| Subscriptions | list of filter + QoS in the profile; default `#` and `$SYS/#` | Live panel: add and remove while connected; granted QoS and SUBACK failure reason; Retry and Remove | P2 | better |
| Message list view | none | Newest-first list (default view), switchable with the tree; sent messages included; All / Received / Sent filter | P3 | better |
| Subscription colours and click-to-filter | none | Automatic colour for each subscription; a click filters the views (Show only) | P3 | better |
| Edit and publish, Reply | none | Copy a message into the composer; reply through the MQTT 5 Response Topic | P5 | better |
| Session | clean start, session expiry, keepalive | Same, plus removal of subscriptions on a persistent session | P2, P4 | done |
| Will message | — | Optional Will, under Advanced in the connection sheet | P4 | better |
| Reconnect | — | Automatic reconnect with backoff (1 s to 30 s) and resubscribe | P2 | better |
| Topic tree | counts, activity blink, staged rendering | Virtual list (own flat rows, ADR 0003), a payload preview, a count of the topics below each row and message counts; no activity dot and no retained marker in the tree (the Messages list has the retained mark) | P1, P3 | partial |
| Topic filter | substring | Substring, debounced, keeps ancestors; also searches the first 1 KiB of the payload in the Messages view | P3 | better |
| Sort | a-z, messages, topics | Tree a–z only. The message list is always chronological. | P3 | dropped (simpler) |
| Pause | stop merge, buffer up to 100 MB | Pause with the store budget, drops shown | P3 | done |
| Value view | JSON/Text | JSON, Text, Hex, Base64, Number (8–64 bit, float, both byte orders); auto from content-type; background formatting; Show all and Show more for long payloads | P5 | better |
| MQTT 5 properties (receive) | user properties only | All properties | P5 | better |
| MQTT 5 properties (publish) | none | content-type, response topic, correlation data, expiry, user properties | P5 | better |
| History | 100 messages or 20 kB per topic | 100 messages per topic (64 KiB each); global budget | P3, P5 | better |
| Publish | topic, payload, QoS, retain, Ctrl+Enter | Always-visible composer, Text and Hex, acknowledgement result, Drafts and Sent recall, Format | P1, P5 | better |
| Clear retained | single topic, confirm | Same | P5 | done |
| Broker statistics (`$SYS`) | special view | Normal topics in the tree | — | dropped (YAGNI) |
| Theme | light/dark | System, Light, Dark override, plus 8 colour presets (see below) | P5 | better |
| Drop counter, memory use | buffer fill percent when paused | Always in the status bar | P1, P3 | better |
| Profile import/export | none | JSON, connection profiles only, no secrets | P4 | better |
| Auto update | none | Signed GitHub Releases updates. Not run on Windows and macOS yet. | P6 | better |
| macOS build | none | arm64 and x64 DMG. Not built in CI yet. | P6 | better |
| Memory limit | fixed buffer | One limit, 32 MiB to 2 GiB, default 128 MiB. Soft limit: batch eviction | P5 | better |
| Theme presets | none | Default, Catppuccin, Solarized, Everforest, Flexoki, Mellifluous, macOS Classic, Tokyo Night. Ayu and Gruvbox wait for a licence check. | P5 | better |
| Command palette | none | `Ctrl/Cmd+K`. Every action is in it. | P5 | better |
| Diagnostics | none | "Open the log folder" and "Copy diagnostics" (no secrets) | P5 | better |
| Update check | none | Once at start, no timer, can be turned off; notice for deb and read-only installs | P6 | better |
| Command line | none | Session connection from options (`--host`, `--tcp`, `--sub`, …) | P1, P4 | better |
| Windows and macOS packages | none | Installer, portable zip, DMG. Built only by the release workflow. | P6 | better |
