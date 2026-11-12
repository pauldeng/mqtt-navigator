# Open questions

Add a question when a decision needs the owner. Move it to "Closed" with the answer and the date.

## Open

1. First release run: the secrets `MINISIGN_SECRET_KEY` and `MINISIGN_PASSWORD` are set (2026-11-11). The Windows files are not signed. Test NSIS, the DMG, the update swap on Windows and macOS, and the attestations.
2. The size gate runs only on Linux. Measure the Windows and macOS binaries from the first release.
3. Upstream: open an issue for `ms-mqtt-client` about the missing Maximum Packet Size check and the unbounded incoming queue. Remove the local patch (ADR 0002) when upstream fixes both.
4. Licence check for the Ayu and Gruvbox themes. Add them only if the licence allows it.
5. A person has not checked the UI in a real window. Every phase report lists UI checks that were not run.
6. The Linux keyring (Secret Service) is not tested (no Secret Service on the development machine). The software renderer was checked on Linux on 2026-11-11: with the Mesa lavapipe Vulkan driver the app starts (first frame 238 ms); with no Vulkan driver it still starts and connects, but which renderer it used was not confirmed (no screenshot). Windows (WARP) is not checked.

## Closed

- 2026-11-11: First CI run on `main` passed on Ubuntu, macOS 15 and Windows 2022 (build, clippy, tests, vendored OpenSSL), plus fuzz and the broker tests.
- 2026-11-11: Updater → our own updater on `ureq`, `minisign-verify` and `sha2`, with a rollback of a bad version (ADR 0005; the separate launcher is gone since 2026-11-12).
- 2026-11-11: Authenticode signing → ship the Windows files unsigned for now. The workflow signs them when the secret `WINDOWS_PFX_BASE64` is set (ADR 0005).
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
- 2026-11-10: gpui-kit `Tree` renders through `uniform_list` (only visible rows).
- 2026-11-11: gpui-kit `Tree` is not used. Its rebuild deep-clones subtrees. We use our own flat rows (ADR 0003). 100k topics at 10k msg/s: worst frame 15 ms.
- 2026-11-11: TLS ALPN (AWS IoT Core on port 443) is possible: build an `openssl` `SslConnectorBuilder` with `set_alpn_protos` and pass it to the client. P2 adds a profile field if needed.
- 2026-11-11: `ms-mqtt-client` does not reject an oversized packet before it allocates. We patch it (ADR 0002).
- 2026-11-10: Binary size → measure in P1, set the gate at the end of P5 (measured + 10%).
