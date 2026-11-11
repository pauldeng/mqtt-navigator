# Phase 5c verification (gpui-kit design guides and themes)

Date: 2026-11-11. Owner requests: follow the gpui-kit design guides (`skills/gpui-kit-design-guides`); offer the applicable gpui-kit themes.

## Done

- Themes: 7 theme files from gpui-kit `themes/` with a verified permissive licence (Catppuccin, Solarized, Everforest, Flexoki, Mellifluous, macOS Classic: MIT; Tokyo Night: Apache-2.0, dark only) are embedded. Presets: Default plus these 7. Title-bar menu with Appearance (System, Light, Dark) and Theme; palette entries. The saved theme is applied before the window opens. `THIRD-PARTY-NOTICES.md` credits each theme. Not bundled: Ayu and Gruvbox (licence not verified), and themes without a clear licence.
- Design-guide fixes: selection with `ToggleGroup`/`Toggle`/`ListItem` instead of primary/ghost swaps; resizable panes with saved widths; rem-based sizes and row heights (zoom); `Empty`, `Tag`, icons; copy fixes (ellipsis, Cancel + verb, sentence case); `docs/ui-design.md` updated.

## Checks run

| Check | Result |
|---|---|
| fmt, clippy (`-D warnings`), cargo-deny, `cargo test --workspace` | pass |
| Release binary | 34 438 104 B (limit 37 317 165 B) |
| Smoke (isolated config, connect) | no error or warning in the log |
| Flood 10k msg/s, 30 s | frame p95 median 10.4 ms, worst second 13.0 ms, merge max 4.0 ms, 0 drops |
| First frame (settings loaded before the window) | 229–272 ms (before: about 200–230 ms) |

## Not checked

- No screenshot or input automation on this Wayland session: layout, pane dragging, menus and the selected states were not looked at.
- The empty states "no subscriptions" and "broker rejected #" are not built (noted in `docs/ui-design.md`).

## Review 1 (Cursor agent, grok-4.7-high): findings and fixes

| Finding | Fix |
|---|---|
| High: Remove and Retry on a subscription row also toggled "Show only" (click bubbled to the row) | Both handlers call `cx.stop_propagation()`. |
| The details pane was not capped by its design maximum (a drag could squeeze the centre, and that width was saved) | `right_max` uses the same rule as the left pane. While expanded, the pane may grow up to the centre minimum; Expand's width is not saved. |
| Low: the Apache-2.0 text was only linked | The notice points to `LICENSE-APACHE` in the repository. |
