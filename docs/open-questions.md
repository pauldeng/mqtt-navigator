# Open questions

Add a question when a decision needs the owner. Move it to "Closed" with the answer and the date.

## Open

1. First release run: the secrets `MINISIGN_SECRET_KEY` and `MINISIGN_PASSWORD` are set (2026-11-11). The Windows files are not signed. Test NSIS, the DMG, the update swap on Windows and macOS, and the attestations.
2. The size gate runs only on Linux. Measure the Windows and macOS binaries from the first release.
3. Licence check for the Ayu and Gruvbox themes. Add them only if the licence allows it.
4. A person has not checked the UI in a real window. Every phase report lists UI checks that were not run.
5. The Linux keyring (Secret Service) is not tested (no Secret Service on the development machine). The software renderer was checked on Linux on 2026-11-11: with the Mesa lavapipe Vulkan driver the app starts (first frame 238 ms); with no Vulkan driver it still starts and connects, but which renderer it used was not confirmed (no screenshot). Windows (WARP) is not checked.
6. Test gaps from the ntex evaluation (phase 8, S5). Each one needs a host set-up that the repo does not have, or a measurement that no test makes:
   - A broker that negotiates only TLS 1.2, an IPv6-only host, and the `getaddrinfo` order for `localhost`.
   - Keep alive 60 s and 70 s while the app publishes. The tests run these two values without publishes, and keep alive 2 s with publishes.
   - The RSS during the 2 MiB-header case. The test checks the time and the text. The evaluation (S1) measured the RSS once.
   - A 3.1.1 load run (10k msg/s) and a load run on EMQX or HiveMQ. S4 measured MQTT 5 on Mosquitto only.
7. What holds the extra memory after an overload (above 10 000 msg/s)? RSS stays about 93 MiB above the 10k plateau until exit. The owner accepted this for 0.2.0. It is bounded and v0.1.1 does the same.
8. Signing-key rotation. `scripts/sign-manifest.sh` verifies with the public key in the repository, so the workflow cannot make the bridge release that an old key must sign (see [Rotate the minisign key](release.md#rotate-the-minisign-key)). Decide how the script gets the old public key for verification before the first key rotation.
9. The system CA store is tested end to end on Linux only (`SSL_CERT_FILE` against Mosquitto). The Windows store and the macOS keychain are not tested with a real broker. A unit test checks that each OS gives at least one root.

## Closed

- 2026-11-15: The fake-broker tests on Windows and macOS → CI passed on Windows, macOS and Ubuntu for the 0.2.0 commit (79124ce).
- 2026-11-15: 0.1.x and a schema 2 settings file → a note is enough. The 0.2.0 CHANGELOG "Known limitations" states it, and the release text copies that section.
- 2026-11-15: RSS after an overload → accepted. After 100k msg/s, RSS stays about 93 MiB above the 10k plateau until the app exits. It is bounded, it does not grow over repeated overloads, and the OS frees it at exit (memory check, phase 8). v0.1.1 does the same. The overload criterion is changed. Finding what holds the extra live memory is a later task.
- 2026-11-15: macOS minimum version → 15.0 (gpui-kit). The packager setting is now 15.0.
- 2026-11-11: First CI run on `main` passed on Ubuntu, macOS 15 and Windows 2022 (build, clippy, tests, vendored OpenSSL), plus fuzz and the broker tests.
- 2026-11-11: Updater → our own updater on `ureq`, `minisign-verify` and `sha2`, with a rollback of a bad version (ADR 0005; the separate launcher is gone since 2026-11-12).
- 2026-11-11: Authenticode signing → ship the Windows files unsigned for now. The workflow signs them when the secret `WINDOWS_PFX_BASE64` is set (ADR 0005).
- 2026-11-10: Update source → public GitHub Releases of `pauldeng/mqtt-navigator`.
- 2026-11-10: Signing → minisign manifest key and Windows Authenticode. No Apple Developer ID.
- 2026-11-10: v1.0 scope → core, MQTT 5 properties, TLS and mTLS.
- 2026-11-10: License → MIT OR Apache-2.0.
- 2026-11-14: MQTT 3.1.1 → supported (owner requirement), default for a new profile. Client library → `ntex-mqtt` (ADR 0006). QoS 2 stays unsupported (product decision, not a library limit).
- 2026-11-10: Rust → latest stable, not pinned.
- 2026-11-10: Scope → two major features, Subscribe and Publish. Tree and message list views. Profiles, TLS/mTLS, MQTT 5 properties, history and auto-update stay.
- 2026-11-10: Proxy, message export, accessibility → not in v1.
- 2026-11-10: Targets → Windows x64, macOS arm64 and x64, Linux x64.
- 2026-11-10: UI → gpui-kit components only, no webview. Theme follows the system or the user choice.
- 2026-11-10: gpui-kit `Tree` renders through `uniform_list` (only visible rows).
- 2026-11-11: gpui-kit `Tree` is not used. Its rebuild deep-clones subtrees. We use our own flat rows (ADR 0003). 100k topics at 10k msg/s: worst frame 15 ms.
- 2026-11-11: TLS ALPN (AWS IoT Core on port 443) is possible: build an `openssl` `SslConnectorBuilder` with `set_alpn_protos` and pass it to the client. P2 adds a profile field if needed.
- 2026-11-11: `ms-mqtt-client` does not reject an oversized packet before it allocates. We patched it (ADR 0002). Closed 2026-11-15: the library is gone (ADR 0006), and the incoming packet limit is checked by ntex-mqtt at the fixed header.
- 2026-11-10: Binary size → measure in P1, set the gate at the end of P5 (measured + 10%).
