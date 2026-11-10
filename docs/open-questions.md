# Open questions

Add a question when a decision needs the owner. Move it to "Closed" with the answer and the date.

## Open

1. P1: Does `ms-mqtt-client` reject an incoming packet that is larger than the advertised Maximum Packet Size before it allocates memory?
2. P1: Does `openssl/vendored` build in CI on Windows and macOS through feature unification?
3. P6: Which updater do we use: `cargo-packager-updater` or a thin custom updater? (ADR 0002)
4. P6: Which Authenticode signing method do we use: a certificate file or Azure Trusted Signing?
5. P2: Does `ms-mqtt-client` support TLS ALPN (needed for AWS IoT Core on port 443)?
6. P1: Does the gpui-kit `Tree` stay within 4 ms per frame with 100k changing topics?

## Closed

- 2026-11-10: Update source → public GitHub Releases of `pauldeng/mqtt-navigator`.
- 2026-11-10: Signing → minisign manifest key and Windows Authenticode. No Apple Developer ID.
- 2026-11-10: v1.0 scope → core, MQTT 5 properties, TLS and mTLS.
- 2026-11-10: License → MIT OR Apache-2.0.
- 2026-11-10: MQTT 3.1.1 and QoS 2 → not supported (library limit).
- 2026-11-10: Rust → latest stable, not pinned.
- 2026-11-10: Scope → two major features, Subscribe and Publish. Tree and message list views. Profiles, TLS/mTLS, MQTT 5 properties, history and auto-update stay.
- 2026-11-10: Proxy, message export, accessibility → not in v1.
- 2026-11-10: Targets → Windows x64, macOS arm64 and x64, Linux x64.
- 2026-11-10: UI → gpui-kit components only, no webview. Theme follows the system or the user choice.
- 2026-11-10: gpui-kit `Tree` renders through `uniform_list` (only visible rows). Its rebuild cost is open question 6.
- 2026-11-10: Binary size → measure in P1, set the gate at the end of P5 (measured + 10%).
