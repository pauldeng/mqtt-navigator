# App icon

- `icon-1024.png`: the master icon, 1024 × 1024 PNG with a transparent background. It shows a compass needle over a branching topic tree.
- Origin: generated on 2026-11-11 with the Codex CLI image-generation tool, from a written brief (no text, no MQTT.org or other brand marks). The owner selected concept A of three.
- Derived files, made from the master with Python Pillow (`Image.resize(.., LANCZOS)`, `Image.save`):
  - `icon-{16,32,48,64,128,256,512}.png`: Linux and packager sizes.
  - `icon.ico`: Windows, sizes 16, 24, 32, 48, 64, 128 and 256.
  - `icon.icns`: macOS, written by the Pillow ICNS writer (all sizes up to 1024).
- `cargo-packager` reads them from `[package.metadata.packager]` in `crates/navigator-app/Cargo.toml`. Make the files again after you change the master.

## Where each platform takes the icon

| Platform | Source | How |
|---|---|---|
| Windows | `icon.ico` | `build.rs` of `navigator-app` embeds it as icon resource ID 1 (`winresource`). gpui loads the window and taskbar icon from the ID 1 resource of the exe. The shortcuts and Explorer use it too. |
| Linux, X11 | `icon-256.png` | The app embeds it and sets `WindowOptions.icon` (`desktop_entry.rs`). |
| Linux, Wayland | `icon-{16..512}.png` | Wayland has no window icon. The shell looks for the desktop entry `io.github.pauldeng.mqtt-navigator.desktop` (the app id) and shows its icon `io.github.pauldeng.mqtt-navigator`. The `.deb` installs both (`[deb.files]` in `crates/navigator-app/Cargo.toml`). The AppImage has only `mqtt-navigator.desktop` and a 512 px icon inside. A bare binary or an AppImage adds a user entry (`~/.local/share/applications`) and icons (48–512 px) on its first start. |
| macOS | `icon.icns` | The packager copies it to `Contents/Resources/icon.icns` and sets `CFBundleIconFile`. A bare binary (not in a `.app`) shows a generic Dock icon. |

The desktop entry is `assets/linux/io.github.pauldeng.mqtt-navigator.desktop`. The packager and the app use the same file.

