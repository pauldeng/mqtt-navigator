# Feature checklist

This list compares MQTT Navigator with the behaviour of mqtt-pathfinder. It also lists improvements. Update the status when a phase finishes.

Status: `todo`, `done`, `dropped` (with a reason), `better` (we do more than the reference).

| Feature | Reference behaviour | Our plan | Phase | Status |
|---|---|---|---|---|
| Connection profiles | name, host, port, mqtt/ws, TLS toggle, client ID | Same, plus TCP/TLS/WS/WSS selector; "Save and connect" tests the profile; session and Will under Advanced | P4 | todo |
| MQTT version | 3.1.1 and 5 | 5 only (library limit) | — | dropped |
| QoS | 0, 1, 2 | 0, 1 (library limit) | — | dropped |
| TLS | separate toggle, off by default; verify **off** by default | TLS **on** by default (port 8883); verify **on** by default; "Not encrypted" warning for TCP/WS; plain-language TLS errors | P2 | better |
| Secrets | OS secure storage, session fallback | System keyring, session fallback | P4 | todo |
| Subscriptions | list of filter + QoS in the profile; default `#` and `$SYS/#` | Live panel: add and remove while connected; granted QoS and SUBACK failure reason | P2 | better |
| Message list view | none | Newest-first list (default view), switchable with the tree; sent messages included; All / Received / Sent filter | P3 | better |
| Subscription colours and click-to-filter | none | Automatic colour for each subscription; a click filters the views | P3 | better |
| Edit and publish, Reply | none | Copy a message into the composer; reply through the MQTT 5 Response Topic | P5 | better |
| Session | clean start, session expiry, keepalive | Same | P2 | todo |
| Will message | — | Optional Will, under Advanced in the connection sheet | P4 | better |
| Topic tree | counts, activity blink, staged rendering | Virtual list, counts, blink, retained marker | P1, P3 | todo |
| Topic filter | substring | Substring, debounced, keeps ancestors | P3 | todo |
| Sort | a-z, messages, topics | Tree a–z only. The message list is always chronological. | P3 | dropped (simpler) |
| Pause | stop merge, buffer up to 100 MB | Pause with the store budget, drops shown | P3 | todo |
| Value view | JSON/Text | JSON, Text, Hex, Base64, Binary numbers; auto from content-type | P5 | better |
| MQTT 5 properties (receive) | user properties only | All properties | P5 | better |
| MQTT 5 properties (publish) | none | content-type, response topic, correlation data, expiry, user properties | P5 | better |
| History | 100 messages or 20 kB per topic | 100 messages per topic; global budget | P3, P5 | better |
| Publish | topic, payload, QoS, retain, Ctrl+Enter | Always-visible composer, acknowledgement result, payload recall | P1, P5 | better |
| Clear retained | single topic, confirm | Same | P5 | todo |
| Broker statistics (`$SYS`) | special view | Normal topics in the tree | — | dropped (YAGNI) |
| Theme | light/dark | Follows the system, manual override | P5 | todo |
| Drop counter, memory use | buffer fill percent when paused | Always in the status bar | P1, P3 | better |
| Profile import/export | none | JSON, connection profiles only, no secrets | P4 | better |
| Auto update | none | Signed GitHub Releases updates | P6 | better |
| macOS build | none | arm64 and x64 DMG | P6 | better |
