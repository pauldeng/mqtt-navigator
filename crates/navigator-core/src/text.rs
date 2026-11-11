//! Safe display of text that a broker or a user controls: topics, filters, property keys and
//! values, payload previews. The strings stay as they are for publish, subscribe and copy. Only
//! the drawn text changes.
//!
//! Direction controls (U+202E "right-to-left override") can make `a/gpj.exe` look like `a/exe.jpg`.
//! Zero-width characters make two different topics look the same. So the UI shows them as
//! `⟨U+202E⟩`.

use std::borrow::Cow;
use std::fmt::Write as _;

/// Text for a topic, a topic level, a filter, a property key or a property value.
///
/// It escapes every control character, direction control, zero-width or other invisible format
/// character (Unicode category Cf and a few look-alikes, for example the soft hyphen, the
/// Hangul fillers and the tag characters). That includes the zero-width joiner, so an emoji
/// sequence in a topic shows its parts. Variation selectors stay after a visible character
/// (U+FE0F makes an emoji) and are escaped when they stand alone. Combining marks stay.
///
/// It borrows the input when nothing needs an escape.
pub fn display_safe(s: &str) -> Cow<'_, str> {
    let mut out: Option<String> = None;
    // The last character can carry a variation selector.
    let mut base = false;
    for (i, c) in s.char_indices() {
        if is_hidden(c) || (is_variation_selector(c) && !base) {
            let o = out.get_or_insert_with(|| s[..i].to_owned());
            let _ = write!(o, "⟨U+{:04X}⟩", c as u32);
            base = false;
        } else {
            if let Some(o) = &mut out {
                o.push(c);
            }
            base = !c.is_whitespace() && !is_variation_selector(c);
        }
    }
    out.map_or(Cow::Borrowed(s), Cow::Owned)
}

/// The escape of one character, as `display_safe` writes it.
pub(crate) fn escape(c: char) -> String {
    format!("⟨U+{:04X}⟩", c as u32)
}

/// A character that can change the direction of the text around it.
pub(crate) fn is_bidi_control(c: char) -> bool {
    matches!(c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

fn is_variation_selector(c: char) -> bool {
    matches!(c, '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}')
}

/// Control characters and invisible format characters.
fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00ad}'                      // soft hyphen
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'                    // Arabic letter mark
            | '\u{06dd}' | '\u{070f}' | '\u{08e2}'
            | '\u{034f}'                    // combining grapheme joiner
            | '\u{115f}' | '\u{1160}'       // Hangul fillers
            | '\u{17b4}' | '\u{17b5}'
            | '\u{180e}'                    // Mongolian vowel separator
            | '\u{200b}'..='\u{200f}'       // zero width, joiners, direction marks
            | '\u{2028}'..='\u{202e}'       // line and paragraph separators, direction controls
            | '\u{2060}'..='\u{206f}'       // word joiner, invisible operators, old controls
            | '\u{3164}' | '\u{ffa0}'       // Hangul fillers
            | '\u{feff}'                    // byte order mark, zero width no-break space
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}' | '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e007f}'     // tag characters
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_borrowed() {
        assert!(matches!(display_safe("a/b/温度/é"), Cow::Borrowed(_)));
        assert!(matches!(display_safe(""), Cow::Borrowed(_)));
    }

    #[test]
    fn right_to_left_override_is_visible() {
        // Without the escape this looks like "invoice/exe.pdf".
        assert_eq!(
            display_safe("invoice/\u{202e}fdp.exe"),
            "invoice/⟨U+202E⟩fdp.exe"
        );
        assert_eq!(display_safe("\u{2066}x\u{2069}"), "⟨U+2066⟩x⟨U+2069⟩");
        assert_eq!(display_safe("a\u{200f}b\u{061c}"), "a⟨U+200F⟩b⟨U+061C⟩");
    }

    #[test]
    fn zero_width_and_invisible_characters_are_visible() {
        assert_eq!(display_safe("ad\u{200d}min"), "ad⟨U+200D⟩min");
        assert_eq!(display_safe("\u{feff}a"), "⟨U+FEFF⟩a");
        assert_eq!(display_safe("a\u{200b}b"), "a⟨U+200B⟩b");
        assert_eq!(display_safe("a\u{00ad}b\u{2060}"), "a⟨U+00AD⟩b⟨U+2060⟩");
        assert_eq!(display_safe("a\u{180e}"), "a⟨U+180E⟩");
        assert_eq!(display_safe("a\u{e0041}"), "a⟨U+E0041⟩");
        assert_eq!(display_safe("a\nb\u{7f}"), "a⟨U+000A⟩b⟨U+007F⟩");
    }

    #[test]
    fn emoji_joiner_sequence_is_escaped_in_a_topic() {
        assert_eq!(
            display_safe("\u{1f468}\u{200d}\u{1f469}"),
            "\u{1f468}⟨U+200D⟩\u{1f469}"
        );
    }

    #[test]
    fn combining_marks_and_emoji_selectors_stay() {
        assert_eq!(display_safe("e\u{301}"), "e\u{301}");
        assert_eq!(display_safe("\u{2764}\u{fe0f}"), "\u{2764}\u{fe0f}");
        assert_eq!(display_safe("\u{1f44d}\u{1f3fd}"), "\u{1f44d}\u{1f3fd}");
    }

    #[test]
    fn standalone_variation_selector_is_escaped() {
        assert_eq!(display_safe("\u{fe0f}a"), "⟨U+FE0F⟩a");
        assert_eq!(display_safe("a \u{fe0f}"), "a ⟨U+FE0F⟩");
        assert_eq!(display_safe("a\u{fe0f}\u{fe0f}"), "a\u{fe0f}⟨U+FE0F⟩");
        assert_eq!(display_safe("\u{200b}\u{fe0f}"), "⟨U+200B⟩⟨U+FE0F⟩");
    }
}
