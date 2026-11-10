# Agent and contributor guide

## Commands

Install the tools (latest stable Rust, latest cargo-deny) with [mise](https://mise.jdx.dev): `mise install`.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
cargo run --release            # start the app
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

- mqtt-pathfinder (CC BY-ND 4.0) and Argus (AGPL-3.0) are references for behaviour only.
- Do not copy their code, text, icons or other assets.

### Engineering

- Do the simplest thing that meets the requirement. Add no dependency without a reason in the PR.
- Use the latest release of every dependency, tool and GitHub Action. `Cargo.lock` records the exact versions. Dependabot proposes updates every week. Do each major upgrade (for example gpui-kit) in its own commit.
- Do not use `unwrap()` in production paths. Use `expect("reason")` only for real invariants.
- Use `thiserror` in `navigator-core` and `anyhow` in `navigator-app`.
- `navigator-core` must not depend on gpui. `unsafe` is forbidden in core. Each `unsafe` block in the app needs a `// SAFETY:` comment.
- QoS 2 must not be expressible. The library does not support it.
- Never block the net thread on the UI. Every queue has a bound.
- Be event-driven. Do not write polling loops or periodic timers that run when the app is idle. A timer must start from an event and stop when its work is done.
- Do no blocking I/O on the UI thread (files, keyring, DNS, certificates, network). Run it in the background and send the result back as an event.
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

- Each phase ends with a report in `docs/plans/phase-N-verification.md` and a read-only Codex review.
- Report only checks that you actually ran.
