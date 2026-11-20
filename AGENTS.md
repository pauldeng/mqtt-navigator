# Agent and contributor guide

## Commands

Install the tools (latest stable Rust, latest cargo-deny) with [mise](https://mise.jdx.dev): `mise install`.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo deny check
cargo run --release            # start the app
```

Broker tests need Docker. They run in parallel. See [tests/broker/README.md](tests/broker/README.md).

```bash
tests/broker/up.sh             # Mosquitto (18830-18838, 18860), EMQX, HiveMQ CE
cargo test -p navigator-core --features broker-tests --test net_mosquitto --test broker_matrix --test broker_compat --test net_keepalive_tasks --test net_system_ca
cargo test -p navigator-app --features broker-tests gui_round_trip
LOAD_WARMUP_SECS=15 LOAD_SECS=60 cargo test --release -p navigator-core --features broker-tests --test load_ci -- --nocapture
tests/broker/down.sh
```

`LOAD_WARMUP_SECS` (default 60) and `LOAD_SECS` (default 300) set the load test time. `up.sh mosquitto` starts only the two Mosquitto services.

Fuzz and soak tests run on a local machine only, not in CI. Run them before a release that changes parsing, the network or the store.

```bash
# Fuzz (nightly Rust and cargo-fuzz). A crash input goes to fuzz/artifacts.
cd fuzz && for t in decode_render:60 parse_hex:20 topic_matches:20 profile_import:60; do cargo +nightly fuzz run "${t%%:*}" -- -max_total_time="${t##*:}" || break; done
# Soak: 1 h at 500 msg/s with 6 broker restarts (SOAK_SECS, SOAK_RATE).
tests/broker/up.sh mosquitto && cargo test --release -p navigator-core --features broker-tests --test soak -- --ignored --nocapture; tests/broker/down.sh
```

## System packages

- Linux (Ubuntu 24.04): `sudo apt install gcc g++ clang pkg-config libfontconfig-dev libwayland-dev libxkbcommon-x11-dev libx11-xcb-dev libssl-dev libzstd-dev libvulkan1 perl make`
- Windows 10+: Visual Studio 2022 Build Tools with "Desktop development with C++", the MSVC Rust toolchain, CMake, Strawberry Perl and NASM (for vendored OpenSSL).
- macOS 15+: Xcode Command Line Tools.

## Rules

### Writing

- Write all documents, comments, UI text and commit messages in ASD-STE100: short sentences, active voice, one instruction for each sentence.
- Use progressive disclosure: README → ARCHITECTURE → `docs/`. Put details in the deepest page that needs them.

### Clean room

- mqtt-pathfinder (CC BY-ND 4.0), Argus (AGPL-3.0) and mqtt-viewer (GPL-3.0) are references for behaviour and ideas only.
- Do not copy their code, text, icons or other assets.

### Engineering

- Do the simplest thing that meets the requirement. Add no dependency without a reason in the PR.
- Use the latest release of every dependency, tool and GitHub Action. `Cargo.lock` records the exact versions. Dependabot proposes updates every week. Do each major upgrade (for example gpui-kit) in its own commit.
- Do not use `unwrap()` in production paths. Use `expect("reason")` only for real invariants.
- Use `thiserror` in `navigator-core` and `anyhow` in `navigator-app`.
- `navigator-core` must not depend on gpui. `unsafe` is forbidden in core. Each `unsafe` block in the app needs a `// SAFETY:` comment.
- QoS 2 must not be expressible. The app does not support it (a product decision; the `Qos` type has two values). A broker QoS 2 message ends the session with a plain text.
- The MQTT client is `ntex-mqtt` (ADR 0006). Use it only in `navigator-core/src/net.rs` and its submodules. Set every deadline in our code. Run the fake-broker and broker tests on each ntex update.
- Never block the net thread on the UI. Every queue has a bound.
- Be event-driven. Do not write polling loops or periodic timers that run when the app is idle. A timer must start from an event and stop when its work is done.
- Do no blocking I/O on the UI thread (files, keyring, DNS, certificates, network). Run it in the background and send the result back as an event.
  - One exception: the panic hook writes the crash file synchronously. A panic on the UI thread ends the app, and a background write would often be lost before the process exits. A contained panic (for example in the net thread) also runs the hook; that write is small, rare and not on the UI thread.
- Keep every UI-thread task within the frame budget (≤ 4 ms of merge work in each frame). Time-slice long work or move it off the UI thread.
- Never log payloads, passwords or keys.

### gpui-kit

- Depend only on `gpui-kit`, with its default features. The only extra feature that may be added is `tree-sitter` (JSON highlighting), and only after P5 measures its size cost and an ADR accepts it. Import with `use gpui_kit::*;`.
- Build the UI from gpui-kit components. Do not use a webview or web UI (`gpui-webview`, `gpui-shell`). Follow [docs/ui-design.md](docs/ui-design.md).
- Use only theme tokens (`cx.theme()`). Do not put raw colour values in components.
- Do not invent APIs. Read the pinned source or <https://gpui-kit.com/component/{name}.md>.
- Call `gpui_kit::init(cx)` before you open a window.
- Use stable `ElementId`s from domain data (for example a topic id), not list indexes.
- Use `RenderOnce` for stateless parts and `Entity<State>` for stateful parts.

### Process

- `main` is protected. Make every change on a branch and merge it with a pull request. Do not push to `main`.
- Each phase ends with a report in `docs/plans/phase-N-verification.md` and a read-only Codex review.
- Report only checks that you actually ran.

### Tests

- Test against a real broker first (`tests/broker/`). Use the fake broker (`crates/navigator-core/tests/common/fake.rs`) only for faults that a real broker does not make. Do not write a second fake of a real broker.
- Each test uses its own topics and client IDs, so the tests run in parallel. Use the `RwLock` of the file for a test that restarts a broker.
- Tests may set short `NetLimits.timers`. Keep one test for each limit with the shipped value.
