# ADR 0007: JSON highlighting in the composer (gpui-kit `tree-sitter`)

Status: accepted (2026-11-20). Owner decision: the composer gets a JSON payload mode with syntax highlighting.

## Context

- The composer has the payload modes Text, Hex and now JSON. JSON mode checks the payload and highlights valid JSON. Invalid JSON gets a warning, and Publish still sends it.
- Highlighting in the gpui-kit `Editor` needs the gpui-kit feature `tree-sitter`. AGENTS.md allowed this feature only after a size measurement and an ADR.

## Measurements (Linux x64, release profile of ADR 0004 at that time: `opt-level = "s"`, `lto = "fat"`, stripped)

| Build | Size |
|---|---|
| Without `tree-sitter` | 35 779 160 B |
| With `tree-sitter` | 36 075 384 B (+296 224 B, +0.8 %) |

The feature adds four crates: `tree-sitter`, `tree-sitter-json`, `tree-sitter-language` and `streaming-iterator`. It adds only the JSON grammar. `cargo deny check` passes. `Cargo.lock` has no `gpui-fast` crate, and wgpu stays at 30.0.2.

## Decision

- Turn on `gpui-kit = { features = ["tree-sitter"] }`. Do not turn on other grammars (`tree-sitter-languages` and the per-language features).
- The composer sets the language `json` only while the payload is valid JSON in JSON mode, and `text` otherwise. It changes the language only when the validity changes.
- The check runs in the background (`decode::json_valid`, no value is built). A result for text that changed since is dropped.

## Consequences

- The binary is 0.8 % larger.
- tree-sitter compiles C code (`cc`), also on Windows and macOS.
