# gpui and gpui-kit reference projects

Researched 2026-11-11 from [awesome-gpui](https://github.com/zed-industries/awesome-gpui). Use these as patterns. Copy code only from permissive licences, keep their headers, and add a NOTICE entry. Do not copy code from GPL or AGPL projects.

## Projects

| Project | Licence | Use it for |
|---|---|---|
| [vicanso/zedis](https://github.com/vicanso/zedis) (Redis GUI) | Apache-2.0 | Theme loading and System mode, resizable layout with saved width, key tree on `List` + `ListDelegate`, command palette, release pipeline, SignPath signing |
| [pavi2410/based](https://github.com/pavi2410/based) (DB client) | MIT | Light/dark theme presets, `cargo-packager` + `cargo-packager-updater` with minisign `latest.json` |
| [fulgur-app/Fulgur](https://github.com/fulgur-app/Fulgur) (editor) | Apache-2.0 | gpui-kit `Settings` page, macOS sign and notarize workflow, notify-only update check with strict URL validation |
| [0xErwin1/dbflux](https://github.com/0xErwin1/dbflux) (DB client) | Apache-2.0 | `ui(px) -> Rems` zoom helper and its test, empty state with key hints, toast throttle |
| [finch-xu/GetCat](https://github.com/finch-xu/GetCat) (API client) | Apache-2.0 | Tiered rendering of large payloads, line index |
| [lassejlv/termy](https://github.com/lassejlv/termy) (terminal) | MIT | DMG signing, AppImage, deb, rpm |
| [zed-industries/zed](https://github.com/zed-industries/zed) `crates/auto_update` | GPL-3.0 | Ideas only: update state machine, never interrupt "Installing", Windows helper process for file swaps |

## Patterns

### Themes

- Embed theme JSON files (`include_str!`) and call `ThemeRegistry::global_mut(cx).load_themes_from_str(..)` for each (Zedis `src/assets.rs`, based `theme/controls.rs`).
- Group light and dark themes as explicit presets `{id, light_name, dark_name}` (based `theme/presets.rs`). Set `Theme::global_mut(cx).light_theme` and `.dark_theme` directly; then `Theme::change(mode)` and `sync_system_appearance` keep working.
- Apply the saved theme before the first window opens, so the first frame has the right theme (Zedis `window_setup.rs`).
- `apply_config` and `Theme::change` reset `font_size`: set the app font size again after each apply (Zedis).
- Persist `{mode: System | Light | Dark, preset_id}`.

### Layout and zoom

- `h_resizable(..).child(resizable_panel().size(saved).size_range(min..max))`; on `on_resize`, keep the width in the view and save it through the debounced writer (Zedis `views/content.rs`).
- Convert design pixels to rem: `const fn ui(px: f32) -> Rems { rems(px / 16.0) }` (DBFlux `crates/bolt/src/tokens.rs`). Compute list row heights from `window.rem_size()` in each render. Test at 1× and 2× base font.

### Lists and trees

- Flat `{id, depth, is_folder}` rows in `List` with a `ListDelegate`; keep the selection in a set keyed by domain id, and prune it after a rebuild (Zedis `views/key_tree/`).

### Large payloads

- Tier A: read-only `Editor` up to a bound; tier B: plain-text `uniform_list` with a line index (`Vec<u32>` of line starts, built with `memchr`, cancellable); tier C: head preview with a visible notice (GetCat `body/tier.rs`, `body/line_index.rs`).

### Other UI

- Empty state: icon, title, one sentence, one primary action, key hints (DBFlux `composites/empty_state.rs`).
- Notifications: gpui-kit `Notification`; throttle warning and info toasts with a token bucket, errors pass (DBFlux `toast.rs`).
- Settings page: gpui-kit `setting::{Settings, SettingPage, SettingGroup, SettingItem}` (Fulgur).
- Keyboard: a shortcuts overlay, shortcuts in tooltips (Zedis `helpers/keybindings.rs`, `views/hotkeys.rs`).

### Release and update (P6)

- Packaging: `cargo-packager` configured in `[package.metadata.packager]` (dmg, nsis, deb, appimage) — based. Check that the tool is maintained when you pin it.
- Updater: `cargo-packager-updater`, minisign public key embedded, private key in a CI secret, `latest.json` on GitHub Releases, Windows `Passive` install mode, deb installs open the releases page instead — based.
- Windows Authenticode: SignPath Foundation (free for open source), with `signtool verify /pa` in CI — Zedis.
- macOS: ad-hoc signing without a Developer ID (based); Developer ID + notarytool + stapler with retries when a certificate exists (Zedis, Fulgur).
- CI: tag-triggered matrix, `fail-fast: false`, a draft release created first, tools pinned with `--locked`, a final job that writes checksums and `latest.json` (based `release.yml`, Zedis `publish.yml`).
