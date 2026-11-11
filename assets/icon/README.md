# App icon

- `icon-1024.png`: the master icon, 1024 × 1024 PNG with a transparent background. It shows a compass needle over a branching topic tree.
- Origin: generated on 2026-11-11 with the Codex CLI image-generation tool, from a written brief (no text, no MQTT.org or other brand marks). The owner selected concept A of three.
- Derived files, made from the master with Python Pillow (`Image.resize(.., LANCZOS)`, `Image.save`):
  - `icon-{16,32,48,64,128,256,512}.png`: Linux and packager sizes.
  - `icon.ico`: Windows, sizes 16, 24, 32, 48, 64, 128 and 256.
  - `icon.icns`: macOS, written by the Pillow ICNS writer (all sizes up to 1024).
- `cargo-packager` reads them from `[package.metadata.packager]` in `crates/navigator-app/Cargo.toml`. Make the files again after you change the master.
