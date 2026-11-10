# Phase 0 verification

Date: 2026-11-10.

## Done

- Documents: README, ARCHITECTURE, AGENTS (+ CLAUDE.md pointer), SECURITY, LICENSE-MIT, LICENSE-APACHE.
- Decisions and lists: `docs/adr/0001-stack.md`, `docs/parity.md`, `docs/open-questions.md`, the plan copy.
- Mockups: `mockups/main-window.html`, `mockups/connection-editor.html` (light and dark, demo states).
- Workspace: `navigator-core` (lib, `Qos` type without QoS 2) and `navigator-app` (binary `mqtt-navigator`, placeholder).
- Tools: `mise.toml` and `rust-toolchain.toml` use the latest stable Rust (owner request). `mise.toml` uses the latest cargo-deny (0.20.2 today). All dependencies use their latest version (owner request).
- CI: `.github/workflows/ci.yml`, with fmt, cargo-deny, a "core has no gpui" check, and clippy and tests on Ubuntu, Windows and macOS.

## Checks run (local, Ubuntu)

| Command | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| `cargo test --workspace` | pass (1 test) |
| `cargo deny check` | pass |
| `cargo run -q` | prints `MQTT Navigator 0.1.0` |

## Not checked

- CI has not run. Nothing is pushed yet.
- The mockups were checked with an HTML parser only. Nobody has looked at them in a browser yet.
- There are no real dependencies yet. P1 adds gpui-kit and ms-mqtt-client.

## Codex review (gpt-6.1-sol, high, read-only)

7 findings. All were fixed:

1. CI actions now use full commit SHAs.
2. The "core has no gpui" check now fails if `cargo tree` fails. It covers all dependency kinds.
3. Mockup: QoS 0 publish shows "Sent", not "Acknowledged".
4. Mockup: topic rows take keyboard focus (arrows, Enter, `aria-selected`).
5. Mockup: Cmd+Enter works on macOS.
6. Mockup: the keyring notes show one state at a time (demo toggle).
7. Plan: the size gate is set at the end of P5 in all places.

The mockup scripts pass `node --check`. The HTML passes the Python HTML parser.

## Later changes and reviews (2026-11-10 to 2026-11-11)

Owner changes: latest stable Rust (not pinned) and latest versions for all dependencies; two major features (Subscribe, Publish); event-driven and responsive rules; gpui-kit components only; System/Light/Dark theme; UI design from MQTT Explorer, MQTTX and mqtt-spy; TLS on by default; two-line rows for long payloads; accessibility, proxy and message export out of v1.

Codex reviews (gpt-6.1-sol, high, read-only):
- Repo gate: 4 more rounds, 25 findings, all fixed.
- UX design: 3 rounds, 18 findings, all fixed. The third round reported no issues.
- Final repo gate: 5 more rounds, 13 findings (mostly mockup behaviour), all fixed. The last round reported no issues.

Mockups: 183 Playwright checks in system Chrome pass (`main-window.html`, `connection-editor.html`). They are design references only.
