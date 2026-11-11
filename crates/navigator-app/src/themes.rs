//! Colour theme presets. Each preset names one light and one dark theme. The theme files are
//! the gpui-kit JSON files in `assets/themes/`, embedded in the binary (see THIRD-PARTY-NOTICES.md).
//! Pattern: based (`theme/presets.rs`, MIT) and Zedis (`window_setup.rs`, Apache-2.0).

use gpui_kit::component::{Theme, ThemeMode as ComponentMode, ThemeRegistry};
use gpui_kit::*;
use navigator_core::settings::ThemeMode;

/// One preset. `id` is what the settings file stores.
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub light_name: &'static str,
    pub dark_name: &'static str,
}

const fn preset(
    id: &'static str,
    label: &'static str,
    light_name: &'static str,
    dark_name: &'static str,
) -> Preset {
    Preset {
        id,
        label,
        light_name,
        dark_name,
    }
}

/// The presets, in menu order. The first one is the default.
pub static PRESETS: &[Preset] = &[
    preset("default", "Default", "Default Light", "Default Dark"),
    preset(
        "catppuccin",
        "Catppuccin",
        "Catppuccin Latte",
        "Catppuccin Mocha",
    ),
    preset(
        "solarized",
        "Solarized",
        "Solarized Light",
        "Solarized Dark",
    ),
    preset(
        "everforest",
        "Everforest",
        "Everforest Light",
        "Everforest Dark",
    ),
    preset("flexoki", "Flexoki", "Flexoki Light", "Flexoki Dark"),
    preset(
        "mellifluous",
        "Mellifluous",
        "Mellifluous Light",
        "Mellifluous Dark",
    ),
    preset(
        "macos-classic",
        "macOS Classic",
        "macOS Classic Light",
        "macOS Classic Dark",
    ),
    // Tokyo Night has no light theme: its light side is the gpui-kit default.
    preset("tokyo-night", "Tokyo Night", "Default Light", "Tokyo Night"),
];

/// The preset with this id. An unknown id gives the default.
pub fn preset_by_id(id: &str) -> &'static Preset {
    PRESETS.iter().find(|p| p.id == id).unwrap_or(&PRESETS[0])
}

/// The embedded theme files.
const FILES: [(&str, &str); 7] = [
    (
        "catppuccin",
        include_str!("../../../assets/themes/catppuccin.json"),
    ),
    (
        "solarized",
        include_str!("../../../assets/themes/solarized.json"),
    ),
    (
        "everforest",
        include_str!("../../../assets/themes/everforest.json"),
    ),
    (
        "flexoki",
        include_str!("../../../assets/themes/flexoki.json"),
    ),
    (
        "mellifluous",
        include_str!("../../../assets/themes/mellifluous.json"),
    ),
    (
        "macos-classic",
        include_str!("../../../assets/themes/macos-classic.json"),
    ),
    (
        "tokyonight",
        include_str!("../../../assets/themes/tokyonight.json"),
    ),
];

/// Loads the embedded theme files into the registry. A file that fails is logged and skipped.
pub fn load_embedded(registry: &mut ThemeRegistry) {
    for (name, text) in FILES {
        if let Err(e) = registry.load_themes_from_str(text) {
            tracing::warn!(file = name, error = %e, "A theme file did not load");
        }
    }
}

/// Registers the themes and applies the saved choice. Call it after `gpui_kit::init` and before
/// the first window opens, so that the first frame has the right colours.
pub fn init(preset_id: &str, mode: ThemeMode, cx: &mut App) {
    load_embedded(ThemeRegistry::global_mut(cx));
    // The default caret blinks every 500 ms for as long as an input has focus. This one stops
    // after 5 s without typing, so a focused idle window sleeps.
    Theme::update(cx, |theme| {
        theme.cursor_blink = gpui_kit::component::input::CursorBlink::blinking(
            std::time::Duration::from_millis(500),
        )
        .with_idle_timeout(std::time::Duration::from_secs(5));
    });
    apply(preset_id, mode, None, cx);
}

/// Sets the light and dark theme of `preset_id`, then the mode: System follows the window
/// appearance, Light and Dark are fixed. A theme that is not in the registry (its file did not
/// load) falls back to the gpui-kit default. The font and the caret settings stay as they were,
/// because a theme change resets some of them.
pub fn apply(preset_id: &str, mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    let p = preset_by_id(preset_id);
    let (light, dark) = {
        let reg = ThemeRegistry::global(cx);
        (
            reg.themes()
                .get(p.light_name)
                .unwrap_or_else(|| reg.default_light_theme())
                .clone(),
            reg.themes()
                .get(p.dark_name)
                .unwrap_or_else(|| reg.default_dark_theme())
                .clone(),
        )
    };
    let pinned = {
        let t = Theme::global_mut(cx);
        t.light_theme = light;
        t.dark_theme = dark;
        (
            t.font_size,
            t.font_family.clone(),
            t.mono_font_family.clone(),
            t.mono_font_size,
            t.cursor_blink,
        )
    };
    match mode {
        ThemeMode::System => Theme::sync_system_appearance(window, cx),
        ThemeMode::Light => Theme::change(ComponentMode::Light, window, cx),
        ThemeMode::Dark => Theme::change(ComponentMode::Dark, window, cx),
    }
    Theme::update(cx, |t| {
        (
            t.font_size,
            t.font_family,
            t.mono_font_family,
            t.mono_font_size,
            t.cursor_blink,
        ) = pinned;
    });
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: the gpui glob import brings its own `test` macro.
    use super::{FILES, PRESETS, load_embedded, preset_by_id};
    use gpui_kit::component::ThemeRegistry;

    #[test]
    fn every_preset_finds_its_themes_in_the_embedded_files() {
        let mut registry = ThemeRegistry::default();
        load_embedded(&mut registry);
        for p in PRESETS.iter().filter(|p| p.id != "default") {
            for name in [p.light_name, p.dark_name] {
                // Tokyo Night borrows the light theme of gpui-kit.
                if name == "Default Light" {
                    continue;
                }
                assert!(
                    registry.themes().contains_key(name),
                    "{} needs the theme {name}",
                    p.id
                );
            }
        }
        assert_eq!(FILES.len(), 7);
    }

    #[test]
    fn ids_are_unique_and_unknown_ids_give_the_default() {
        let mut ids: Vec<_> = PRESETS.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), PRESETS.len());
        assert_eq!(preset_by_id("nope").id, "default");
        assert_eq!(preset_by_id("flexoki").label, "Flexoki");
    }
}
